"""Finite registry recording lifecycle with stubbed owners and one stub assessor."""
import copy
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import recording_contract as contract
import recording_jobs as jobs
import registry_recording
from fixture_recording import RecordingFixture, SESSION
from store_registry import resolve

PERSONAL, COMPANY, SCRATCH, ARCHIVE, NODE = ("01ARZ3NDEKTSV4RRFFQ69G5FA" + suffix for suffix in "VWXYZ")


class Runtime:
    def __init__(self, fixture, destination="team-notes", *, associate=False, after=None):
        self.fixture, self.destination, self.associate, self.after = fixture, destination, associate, after
        self.calls = []

    def assess(self, observation, overlaps, *, timeout, **options):
        self.fixture.assertEqual(self.fixture.state()["jobs"][0]["phase"], "assessing")
        prompt, context = contract.prepare(observation, overlaps, **options)
        self.fixture.assertIsNotNone(prompt, context)
        self.calls.append((prompt, context, options, timeout))
        store = next(store for store in context.selected_stores if store.name == self.destination)
        target = next((target for target in context.association_bindings if target.db_id == store.db_id), None)
        proposal = {"kind": "lesson", "summary": "User chose R5.",
                    "body": "The user corrected the target to R5.",
                    "evidence_ids": [context.bindings[0].evidence_id],
                    "destination": self.destination, "tags": [],
                    "associate_with": target.opaque_id if self.associate and target else None}
        answer = contract.validate_answer({"proposal": proposal}, context)
        if self.after:
            self.after()
        return {"reason": "proposed", "provider_attempt": True,
                "usage": {"input_tokens": 20, "output_tokens": 5}, **answer}


class RegistryRecordingTests(unittest.TestCase, RecordingFixture):
    def setUp(self):
        RecordingFixture.__init__(self, self)
        self.root = self.root.resolve()
        self.registry = self.root / "stores.json"
        self.entries = [self.entry("across-work", PERSONAL, "user", personal=True),
                        self.entry("team-notes", COMPANY, "company", remote=True),
                        self.entry("workbench", SCRATCH, "project"),
                        self.entry("archive", ARCHIVE, "user", record=False)]
        self.write_registry()
        self.config.pop("service_config")  # No invented primary owner for an unconfigured workspace.
        self.config.update(schema="mneme.codex-hooks.config.v12", memory_scope="registry",
                           project_root=self.root, memory_mode="async")
        self.refresh()
        self.identity.side_effect = self.identity_result
        self.overlap.side_effect = self.overlap_result

    def entry(self, name, identifier, alias, *, personal=False, remote=False, record=True):
        path = self.root / (name + ".json")
        service = {"mode": "remote" if remote else "connect", "database_name": alias,
                   "database_path": "/srv/mneme/" + name + ".db",
                   "url": "ssh://memory@example.test" if remote else "http://127.0.0.1:18766/"}
        if remote:
            service["schema"] = 1
        path.write_text(json.dumps(service))
        return {"name": name, "service_config": str(path), "db_alias": alias,
                "database_path": service["database_path"], "db_id": identifier,
                "purpose": "User-authored cross-project collaboration preferences." if personal else "Scoped team decisions.",
                "read": {"enabled": True},
                "record": {"enabled": record,
                    "kinds": ["lesson"] if personal else ["lesson", "episode", "possibility"],
                    "evidence_kinds": ["user_statement"] if personal else list(contract.KINDS),
                    "required_tags": ["collaboration-preference"] if personal else [],
                    "allow_associations": not personal, "max_summary_bytes": 384 if personal else 512,
                    "max_body_bytes": 1024 if personal else 2048},
                "maintenance": {"enabled": False}}

    def write_registry(self):
        self.registry.write_text(json.dumps({"schema": "mneme.stores.v1", "stores": self.entries}))

    def refresh(self):
        self.config["frozen_selection"] = resolve(self.root, registry_path=self.registry).to_dict()

    def identity_result(self, service, root, **kwargs):
        policy = kwargs["store_target"]
        return {"outcome": "ok", "db_id": policy.db_id,
                "native_work": {"decoded_bytes": kwargs["spent_native_bytes"] + 100}}

    def overlap_result(self, service, cue, root, **kwargs):
        return {"outcome": "empty", "db_id": kwargs["expected_db_id"], "cards": [],
                "native_work": {"decoded_bytes": kwargs["spent_native_bytes"] + 100}}

    def native_write(self, config, job, timeout):
        self.assertEqual(self.state()["jobs"][0]["phase"], "write_intent")
        registry_recording.validate_payload(config, job)
        self.writes.append((config, copy.deepcopy(job), timeout))
        return {"db": job["db_alias"], "db_id": job["db_id"], "id": NODE,
                "readback_status": "verified", "replayed": False}

    def test_three_destinations_one_assessment_remote_owner_and_shared_budget(self):
        self.closed()
        runtime = Runtime(self)
        self.step(runtime)
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(len(self.reservations), 1)
        self.assertEqual(len(self.accounts), 1)
        self.assertEqual(len(self.writes), 1, self.state())
        job = self.writes[0][1]
        self.assertEqual((job["destination"], job["db_alias"], job["db_id"]), ("team-notes", "company", COMPANY))
        self.assertEqual([call.kwargs["spent_native_bytes"] for call in self.identity.call_args_list], [0, 100, 200])
        self.assertEqual([call.kwargs["spent_native_bytes"] for call in self.overlap.call_args_list], [300, 400])
        context = runtime.calls[0][1]
        self.assertEqual([store.name for store in context.selected_stores], ["across-work", "team-notes", "workbench"])
        self.assertNotIn("archive", json.dumps(contract.schema_for(context)))
        receipt = self.state()["receipts"][-1]
        self.assertEqual(receipt["target"]["db_id"], COMPANY)
        self.assertEqual(receipt["store_context"]["offered"], 3)
        self.assertEqual(receipt["store_context"]["routing"], "not_enabled")

    def test_renamed_personal_restrictions_are_data_not_special_lane(self):
        self.closed()
        runtime = Runtime(self, "across-work")
        self.step(runtime)
        self.assertEqual(len(self.writes), 1, self.state())
        job = self.writes[0][1]
        self.assertEqual(job["payload"]["tags"], ["collaboration-preference"])
        self.assertEqual(job["proposal_kind"], "lesson")
        self.assertTrue(all(item["kind"] == "user_statement" for item in job["citations"]))
        self.assertNotIn("links", job["payload"])
        self.assertNotIn("maintenance", job)
        self.assertEqual(job["payload"]["source"]["namespace"], "codex-acquisition.v1")

    def test_unavailable_destination_removed_without_fallback_or_retry(self):
        def identity(service, root, **kwargs):
            value = self.identity_result(service, root, **kwargs)
            if kwargs["store_target"].db_id == COMPANY:
                value.update(outcome="unavailable")
            return value
        self.identity.side_effect = identity
        self.closed()
        runtime = Runtime(self, "workbench")
        self.step(runtime)
        self.assertEqual([store.name for store in runtime.calls[0][1].selected_stores], ["across-work", "workbench"])
        self.assertEqual(self.writes[0][1]["db_id"], SCRATCH)
        self.assertEqual(self.state()["receipts"][-1]["store_context"]["identity_unavailable"], 1)

    def test_no_available_destination_spends_no_model_attempt(self):
        self.identity.side_effect = lambda *args, **kw: {"outcome": "unavailable",
            "native_work": {"decoded_bytes": kw["spent_native_bytes"] + 100}}
        self.closed()
        runtime = Runtime(self)
        self.step(runtime)
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.reservations, [])
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["receipts"][-1]["reason"], "no_available_destination")

    def test_excluded_store_gets_neither_identity_nor_query(self):
        excluded = self.entry("not-for-here", "01ARZ3NDEKTSV4RRFFQ69G5FB0", "user")
        excluded["when"] = {"excluded_roots": [str(self.root)]}
        Path(excluded["service_config"]).unlink()  # Admission must precede any access to it.
        self.entries.append(excluded)
        self.write_registry(); self.refresh()
        self.closed()
        runtime = Runtime(self)
        self.step(runtime)
        self.assertEqual(len(self.writes), 1, self.state())
        self.assertTrue(all("not-for-here" not in str(call) for call in self.identity.call_args_list + self.overlap.call_args_list))
        self.assertNotIn("not-for-here", runtime.calls[0][0])

    def test_unknown_native_charge_stops_other_reads_and_does_not_invent_empty_owner(self):
        self.identity.side_effect = lambda *args, **kw: {"outcome": "unavailable",
            "native_work": {"decoded_bytes": None, "known_decoded_bytes": kw["spent_native_bytes"]}}
        self.closed()
        self.step(Runtime(self))
        self.assertEqual(self.identity.call_count, 1)
        self.overlap.assert_not_called()
        self.assertEqual(self.reservations, [])
        self.assertEqual(self.state()["receipts"][-1]["store_context"]["budget_omitted"], 2)

    def changed_after_assessment(self, field, value):
        self.closed()
        def change():
            self.entries[1][field] = value
            self.write_registry()
        runtime = Runtime(self, after=change)
        self.step(runtime)
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(len(self.reservations), 1)
        self.assertEqual(len(self.accounts), 1)
        self.assertEqual(self.writes, [])
        self.refresh()
        self.assertFalse(self.step(runtime)[0])
        self.assertEqual(len(self.reservations), 1)
        self.assertEqual(self.state()["counts"]["assessed"], 1)

    def test_purpose_change_after_assessment_cancels_without_refund(self):
        self.changed_after_assessment("purpose", "A different capture purpose.")

    def test_owner_rebind_after_assessment_cancels_without_refund(self):
        self.changed_after_assessment("db_id", "01ARZ3NDEKTSV4RRFFQ69G5FB1")

    def test_same_node_id_association_is_owner_qualified_and_uses_remaining_budget(self):
        def overlap(service, cue, root, **kwargs):
            return {"outcome": "ok", "db_id": kwargs["expected_db_id"],
                    "cards": [{"id": NODE, "kind": "semantic", "summary": "Prior scoped decision."}],
                    "native_work": {"decoded_bytes": kwargs["spent_native_bytes"] + 100}}
        self.overlap.side_effect = overlap
        self.closed()
        with patch("hook_recall.check_association_target", return_value={"outcome": "kept",
                    "native_work": {"decoded_bytes": 550}}) as check:
            self.step(Runtime(self, associate=True))
        self.assertEqual(len(self.writes), 1, self.state())
        self.assertEqual(check.call_args.kwargs["spent_native_bytes"], 500)
        self.assertTrue(check.call_args.kwargs["with_work"])
        self.assertEqual(check.call_args.kwargs["expected_db_id"], COMPANY)
        job = self.writes[0][1]
        self.assertEqual(job["association"]["db_id"], COMPANY)
        self.assertEqual(job["payload"]["links"][0]["to"], NODE)

    def test_native_write_uses_explicit_ssh_transport_and_identity_guard(self):
        self.closed()
        self.step(Runtime(self))
        self.assertEqual(len(self.writes), 1, self.state())
        job = self.writes[0][1]
        with patch("mcp_client.McpClient") as factory:
            client = factory.return_value
            client.call_tool.return_value = [{"db": "company", "name": "company", "state": "open",
                "db_id": COMPANY, "configured_path": self.entries[1]["database_path"]}]
            client.save_verified.return_value = {"db": "company", "db_id": COMPANY, "id": NODE,
                "readback_status": "verified"}
            receipt = jobs._native_write(self.config, job, 2)
            self.assertEqual(receipt["db_id"], COMPANY)
            self.assertTrue(factory.call_args.kwargs["remote_owner"])
            self.assertEqual(factory.call_args.args[0], "ssh://memory@example.test")
            self.assertEqual(client.save_verified.call_args.args[0], "company")
            self.assertEqual(client.save_verified.call_args.kwargs["expected_db_id"], COMPANY)

    def test_unknown_write_keeps_historical_owner_after_rebind(self):
        self.closed()
        def ambiguous(config, job, timeout):
            self.entries[1]["db_id"] = "01ARZ3NDEKTSV4RRFFQ69G5FB1"
            self.write_registry()
            raise TimeoutError("lost native acknowledgement")
        self.step(Runtime(self), native_write=ambiguous)
        state = self.state()
        self.assertEqual(state["jobs"][0]["phase"], "unresolved")
        self.assertEqual(state["jobs"][0]["db_id"], COMPANY)
        self.assertEqual(state["jobs"][0]["reason"], "native_write_ambiguous")
        # No live selection access or present-day owner reconstruction is needed
        # to report the retained unknown outcome, even while old config is stale.
        status = jobs.drain_status(self.config, [SESSION])
        self.assertEqual(status["unresolved"], 1)
        exported = jobs.export_receipts(self.config, [SESSION])
        self.assertTrue(exported["unknown"])
        self.refresh()
        self.assertFalse(self.step(Runtime(self))[0])
        self.assertEqual(self.state()["jobs"][0]["db_id"], COMPANY)
        self.assertEqual(len(self.reservations), 1)

    def test_changed_service_after_admission_defers_before_inference(self):
        self.closed()
        service = Path(self.entries[1]["service_config"])
        data = json.loads(service.read_text())
        data["url"] = "ssh://memory@replacement.test"
        service.write_text(json.dumps(data))
        runtime = Runtime(self)
        self.step(runtime)
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.reservations, [])
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["receipts"][-1]["reason"], "config_changed")

    def test_injected_project_pins_first_native_id_and_preserves_serialized_selection(self):
        from store_registry import project_store
        project_id = "01ARZ3NDEKTSV4RRFFQ69G5FB2"
        project_entry = self.entry("enrolled", project_id, "project")
        binding = project_store(project_entry["service_config"], self.root,
            database_path=project_entry["database_path"], purpose="Project capture policy.",
            read=project_entry["read"], record=project_entry["record"])
        self.config.update(memory_scope="project", service_config=Path(project_entry["service_config"]),
                           _project_focus=None)
        self.config["frozen_selection"] = resolve(self.root, registry_path=self.registry,
                                                  project_binding=binding).to_dict()
        def identity(service, root, **kwargs):
            value = self.identity_result(service, root, **kwargs)
            if value["db_id"] is None:
                value["db_id"] = project_id
            return value
        self.identity.side_effect = identity
        self.closed()
        self.step(Runtime(self, "project"))
        self.assertEqual(len(self.writes), 1, self.state())
        self.assertEqual(self.writes[0][1]["db_id"], project_id)
        self.assertIsNone(self.writes[0][1]["store_target"]["db_id"])
        self.assertEqual(self.state()["receipts"][-1]["target"]["db_id"], project_id)

    def test_exhausted_identity_budget_does_not_grant_next_owner_fresh_allowance(self):
        self.identity.side_effect = lambda *args, **kw: {"outcome": "unavailable",
            "native_work": {"decoded_bytes": kw["budget"].native_read_bytes + 1}}
        self.closed()
        self.step(Runtime(self))
        self.assertEqual(self.identity.call_count, 1)
        self.overlap.assert_not_called()
        self.assertEqual(self.reservations, [])
        self.assertEqual(self.state()["receipts"][-1]["store_context"]["budget_omitted"], 2)

    def test_store_cue_not_task_prompt_goes_to_that_owner(self):
        self.entries[1]["read"]["cue"] = "Fixed team-memory selection question."
        self.write_registry(); self.refresh()
        self.closed()
        self.step(Runtime(self))
        self.assertEqual(self.overlap.call_args_list[0].args[1], "Fixed team-memory selection question.")
        self.assertEqual(self.overlap.call_args_list[1].args[1], "no, R5")

    def test_write_only_policy_never_queries_historical_payloads_or_offers_links(self):
        self.entries[1]["read"]["enabled"] = False
        self.write_registry(); self.refresh()
        self.closed()
        runtime = Runtime(self)
        self.step(runtime)
        self.assertEqual(len(self.writes), 1, self.state())
        self.assertTrue(all(call.kwargs["expected_db_id"] != COMPANY for call in self.overlap.call_args_list))
        store = next(store for store in runtime.calls[0][1].selected_stores if store.name == "team-notes")
        self.assertFalse(store.allow_associations)

    def test_final_identity_read_has_no_fresh_byte_allowance(self):
        from librarian_policy import resolve as budget
        self.closed(); self.step(Runtime(self))
        original = self.writes[0][1]
        for charge in (None, budget(self.config).native_read_bytes, budget(self.config).native_read_bytes + 1):
            job = {**original, "registry_native_bytes": charge}
            with self.subTest(charge=charge), patch("mcp_client.McpClient") as factory:
                with self.assertRaises(registry_recording.ReadBudgetRefused):
                    jobs._native_write(self.config, job, 2)
                factory.assert_not_called()
        job = {**original, "registry_native_bytes": budget(self.config).native_read_bytes - 1}
        with patch("mcp_client.McpClient") as factory:
            client = factory.return_value
            client.call_tool.return_value = [{"db": "company", "name": "company", "state": "open",
                "db_id": COMPANY, "configured_path": self.entries[1]["database_path"]}]
            with self.assertRaises(registry_recording.ReadBudgetRefused):
                jobs._native_write(self.config, job, 2)
            client.save_verified.assert_not_called()

    def test_unknown_association_read_charge_stops_before_write_intent(self):
        self.overlap.side_effect = lambda *args, **kw: {"outcome": "ok", "db_id": kw["expected_db_id"],
            "cards": [{"id": NODE, "kind": "semantic", "summary": "Prior decision."}],
            "native_work": {"decoded_bytes": kw["spent_native_bytes"] + 100}}
        self.closed()
        with patch("hook_recall.check_association_target", return_value={"outcome": "unavailable",
                    "native_work": {"decoded_bytes": None, "known_decoded_bytes": 500}}):
            self.step(Runtime(self, associate=True))
        self.assertEqual(self.writes, [])
        self.assertEqual(len(self.accounts), 1)
        self.assertEqual(self.state()["receipts"][-1]["reason"], "native_read_budget")

    def test_verified_write_stays_verified_when_permission_changes_after_acceptance(self):
        self.closed()
        with patch("mcp_client.McpClient") as factory:
            client = factory.return_value
            client.call_tool.return_value = [{"db": "company", "name": "company", "state": "open",
                "db_id": COMPANY, "configured_path": self.entries[1]["database_path"]}]
            def accepted(*args, **kwargs):
                self.entries[1]["record"]["enabled"] = False
                self.write_registry()
                return {"db": "company", "db_id": COMPANY, "id": NODE,
                        "readback_status": "verified", "replayed": False}
            client.save_verified.side_effect = accepted
            self.step(Runtime(self), native_write=jobs._native_write)
            client.save_verified.assert_called_once()
        self.assertEqual(self.state()["jobs"], [])
        receipt = self.state()["receipts"][-1]
        self.assertEqual(receipt["outcome"], "verified")
        self.assertEqual(receipt["target"]["db_id"], COMPANY)
        self.assertEqual(jobs.export_receipts(self.config, [SESSION])["receipts"][0]["outcome"], "verified")
        self.refresh()
        self.assertFalse(self.step(Runtime(self))[0])
        self.assertEqual(len(self.accounts), 1)


if __name__ == "__main__":
    unittest.main()

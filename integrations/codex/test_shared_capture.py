"""Synthetic host-neutral capture/component fixtures; no installed host or provider."""
import copy
from contextlib import contextmanager
import hashlib
import json
import os
import subprocess
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import capture_source
import recording_contract as contract
import recording_jobs as jobs
import reader_worker as reader
from rollout_primitives import encoded
import fixture_source_turn as source_fixture


def reference(ordinal):
    return {"ordinal": ordinal, "line": ordinal + 1, "ordinal_scope": "source_turn",
            "byte_offset": ordinal * 256, "line_bytes": 128,
            "raw_line_sha256": hashlib.sha256(f"record-{ordinal}".encode()).hexdigest(),
            "item_id": f"synthetic-item-{ordinal}"}


def observation(items=None):
    # Reuse source test support, never import a runnable test suite as a fixture.
    checks = unittest.TestCase()
    fixture = source_fixture.SourceTurnFixture(checks)
    try:
        fixture.write(source_fixture.opening() + [source_fixture.assistant(), source_fixture.event("task_complete")])
        value = fixture.observe()
    finally:
        checks.doCleanups()
    if items is not None:
        value["evidence"] = items
        value["coverage"]["public_evidence"] = {"mode": "all", "observed_records": len(items),
                                               "selected_records": len(items), "omitted_records": 0}
        value["work"]["evidence_bytes"] = len(encoded(items))
    return value


def neutral_observation(*, legacy=False):
    def ref(ordinal):
        return {**reference(ordinal), "ordinal_scope": "source_window"}
    items = [
        {"kind": "user_statement", "ref": ref(0), "evidence_index": 0,
         "source_field": "/message/content/0/text", "content": [{"content_index": 0, "text": "Inspect this setting."}]},
        {"kind": "assistant_assertion", "phase": "commentary", "ref": ref(1), "evidence_index": 1,
         "source_field": "/message/content/0/text", "content": [{"content_index": 0, "text": "I'll inspect it."}]},
        {"kind": "tool_call", "ref": ref(1), "evidence_index": 2,
         "source_field": "/message/content/1", "type": "tool_use", "call_id": "tool-A", "name": "Read", "input": {"path": "setting"}},
        {"kind": "tool_result", "ref": ref(2), "evidence_index": 3,
         "source_field": "/message/content/0/content", "type": "tool_result", "call_id": "tool-A", "output": "setting=5"},
        {"kind": "assistant_assertion", "phase": "final_answer", "ref": ref(3), "evidence_index": 4,
         "source_field": "/message/content/0/text", "content": [{"content_index": 0, "text": "The tool reported setting=5."}]},
        {"kind": "assistant_assertion", "phase": "final_answer", "ref": ref(3), "evidence_index": 5,
         "source_field": "/message/content/1/text", "content": [{"content_index": 1, "text": "Suitability remains untested."}]},
    ]
    value = observation(items)
    value.update(schema="mneme.capture-turn-observation.v1" if legacy else capture_source.OBSERVATION_SCHEMA,
                 source_host="claude_code", boundary=ref(3))
    value["coverage"]["prompt_authority"] = "unverified"
    if not legacy:
        value.update(status="observed", reason="source_snapshot_ready")
        value["coverage"].update(snapshot={
            "schema": "mneme.capture-source-snapshot.v1", "trigger": "session_end",
            "observed_through": ref(3), "observed_sha256": hashlib.sha256(b"synthetic complete-line prefix").hexdigest(),
            "latest_stop": "matched", "trailing_partial_line": False, "unmatched_calls": 0})
        value["coverage"].update(source_turn="observed_prefix", turn_finality="unproven", remaining_transcript="unknown")
    return value


class SharedEvidenceTests(unittest.TestCase):
    def test_raw_line_and_logical_blocks_remain_distinct(self):
        value = neutral_observation()
        prompt, context = contract.prepare(value)
        self.assertIsInstance(context, contract.ValidationContext, context)
        self.assertEqual(len(context.bindings), 6)
        self.assertEqual(context.tool_pairs, (("t001_call", "t001_result"),))
        self.assertEqual(context.bindings[1].source_ref_json, context.bindings[2].source_ref_json)
        self.assertNotEqual(context.bindings[1].source_field, context.bindings[2].source_field)
        self.assertEqual(context.boundary_ref_json, context.bindings[-1].source_ref_json)
        self.assertEqual(context.source_host, "claude_code")
        self.assertFalse(context.root_user_authority)
        self.assertIn("authorship is unverified", prompt)
        self.assertEqual(json.loads(context.bindings[2].text), {"name": "Read", "input": {"path": "setting"}})
        self.assertEqual(value, neutral_observation())  # Packing never edits physical anchors.

    def test_unknown_host_version_order_and_forged_coordinates_refuse(self):
        variants = []
        for key, value in (("source_host", "other"), ("schema", "mneme.capture-turn-observation.v99")):
            item = neutral_observation(); item[key] = value; variants.append(item)
        item = neutral_observation(); item["evidence"][2]["evidence_index"] = 1; variants.append(item)
        item = neutral_observation(); item["evidence"][2]["source_field"] = item["evidence"][1]["source_field"]; variants.append(item)
        item = neutral_observation(); item["evidence"][2]["ref"]["raw_line_sha256"] = "a" * 64; variants.append(item)
        item = neutral_observation(); item["evidence"][2]["source_field"] = "/bad~escape"; variants.append(item)
        item = neutral_observation(); item["evidence"][2]["type"] = "function_call"; variants.append(item)
        for item in variants:
            with self.subTest(item=item):
                self.assertIsNone(contract.prepare(item)[0])

    def test_codex_v3_rejects_neutral_extensions_and_same_line_completion(self):
        for mutate in (lambda o: o.update(source_host="claude_code"),
                       lambda o: o["evidence"][0].update(evidence_index=0),
                       lambda o: o.update(boundary=o["evidence"][-1]["ref"])):
            value = observation(); mutate(value)
            self.assertIsNone(contract.prepare(value)[0])
        prompt, context = contract.prepare(observation())
        self.assertIsNotNone(prompt)
        self.assertTrue(context.root_user_authority)
        self.assertEqual(context.source_host, "codex")

    def test_concern_coordinates_host_qualified(self):
        _, context = contract.prepare(neutral_observation())
        b = context.bindings[0]
        citation = contract.Citation(b.evidence_id, b.kind, b.source_ref_json, b.source_field, b.rendering)
        self.assertTrue(contract.concern_evidence([citation], context, session="s", turn="t")[0]["source_ref"].startswith("claude-code-evidence-reference:sha256:"))

    def test_observed_prefix_contract_and_legacy_v1_have_distinct_coverage(self):
        value = neutral_observation()
        prompt, context = contract.prepare(value)
        self.assertIsNotNone(prompt, context)
        self.assertEqual(json.loads(context.coverage_json), value["coverage"])
        self.assertEqual(json.loads(context.boundary_ref_json), value["coverage"]["snapshot"]["observed_through"])
        self.assertEqual(value["coverage"]["turn_finality"], "unproven")
        legacy = neutral_observation(legacy=True)
        legacy_prompt, legacy_context = contract.prepare(legacy)
        self.assertIsNotNone(legacy_prompt, legacy_context)
        self.assertEqual(json.loads(legacy_context.coverage_json)["source_turn"], "closed_verified")
        self.assertNotIn("snapshot", json.loads(legacy_context.coverage_json))
        malformed = copy.deepcopy(value)
        malformed["coverage"]["source_turn"] = "closed_verified"
        self.assertIsNone(contract.prepare(malformed)[0])


class SharedLifecycleTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.service = self.root / "service.json"
        self.service.write_text("{}")
        self.codex = {"state_dir": self.root / "state", "service_config": self.service,
                      "project_root": self.root, "recording_mode": "automatic",
                      "reader_model": "gpt-6.1-sol", "librarian_effort": "medium"}
        self.claude = {**self.codex, "schema": "mneme.claude-hooks.config.v1",
                       "source_host": "claude_code", "assessor": "claude_code",
                       "reader_model": "claude-sonnet-4-6", "recall_mode": "off"}

    def test_shared_observation_preserves_each_host_contract(self):
        admission, delivery, observed = object(), object(), object()
        limits = jobs.Limits(scan_bytes=256 * 1024, events=512)
        common = {"limits": limits, "delivery": delivery}
        snapshot_options = {"stop_receipt": object(), "boundary_receipt": object(),
                            "snapshot": object(), "allow_partial": True}
        for config, module, expected in (
            (self.codex, "turn_observer", common),
            (self.claude, "claude_observer", {**common, **snapshot_options}),
        ):
            with self.subTest(host=module), patch(
                    module + ".observe_source_turn", autospec=True, return_value=observed) as observe:
                result = capture_source.observe(config, admission, **common, **snapshot_options)
                self.assertIs(result, observed)
                observe.assert_called_once_with(admission, **expected)
                self.assertIs(observe.call_args.kwargs["limits"], limits)

    def test_host_state_schema_and_namespace_never_alias_codex(self):
        self.assertNotEqual(jobs._path(self.codex, "same"), jobs._path(self.claude, "same"))
        self.assertNotEqual(reader._paths(self.codex, "same"), reader._paths(self.claude, "same"))
        for config, other in ((self.codex, self.claude), (self.claude, self.codex)):
            ledger = jobs._new("same", config=config)
            self.assertTrue(jobs._valid(ledger, "same", config))
            self.assertFalse(jobs._valid(ledger, "same", other))
            self.assertTrue(reader._valid(reader._new(config), config))
            self.assertFalse(reader._valid(reader._new(config), other))
        with self.assertRaises(ValueError):
            capture_source.directory({**self.claude, "source_host": "unknown"}, "reader")
        with self.assertRaises(ValueError):
            capture_source.directory({**self.claude, "schema": "mneme.claude-hooks.config.v99"}, "reader")

    def test_capture_only_prompt_never_schedules_selector_or_stewardship(self):
        event = {"session_id": "s", "turn_id": "t", "prompt": "a substantive request"}
        self.assertEqual(reader.notice(self.claude, event, True), {"outcome": "skipped"})
        state = json.loads(reader._paths(self.claude, "s")[0].read_bytes())
        self.assertIsNone(state["pending"])
        # Even an accidentally retained pending payload cannot reach selector.
        reader._state(self.claude, "s", lambda d: (d.update(pending={"turn": "t"}), True))
        with patch("recording_jobs.has_work", return_value=False), patch("stewardship.step") as maintenance:
            factory = unittest.mock.Mock(side_effect=AssertionError("no runtime should be constructed"))
            reader.serve(self.claude, "s", runtime_factory=factory, idle_seconds=.08)
        factory.assert_not_called()
        maintenance.assert_not_called()

    def test_genuine_prior_codex_job_is_retained_not_reinterpreted(self):
        # Produced by _new/notice/close_turn from f516d62dda79c3ad86dc79d6b3ef6ef48c56728a.
        # Prior recording_jobs.py SHA256:
        # 01ccb291e3d34150bb356823184e636548286add317382175e0efc7c4a9c93e7
        # Regenerated by fixtures/generate_codex_recording_v1_fixture.py with
        # deterministic synthetic observer/environment inputs BEFORE proof hashing.
        # Not captured host bytes, not post-hoc normalization, and no migration.
        path = Path(__file__).parent / "fixtures" / "codex-recording-v1-f516d62.json"
        raw = path.read_bytes()
        self.assertEqual(hashlib.sha256(raw).hexdigest(), "26b73f57c086ef9fd389dbc18c1e2751853e6f10962729383423c6b1fa10e994")
        value = json.loads(raw)
        self.assertEqual(value["pin"]["path"], "/synthetic/codex/sessions/recording-session.jsonl")
        self.assertEqual((value["pin"]["device"], value["pin"]["inode"]), (1, 2))
        self.assertEqual(value["jobs"][0]["boot_id"], "synthetic-boot-witness")
        self.assertTrue(jobs._valid(value, value["session_id"], self.codex))
        self.assertFalse(jobs._valid(value, value["session_id"], self.claude))
        destination = self.root / "retained.json"
        jobs._write(destination, copy.deepcopy(value))
        self.assertEqual(jobs._load(destination, value["session_id"], self.codex), value)
        self.assertNotIn("source_host", value["jobs"][0])
        self.assertNotIn("assessor", value["jobs"][0])

    def test_genuine_preserved_v1_reader_refuses_v2_snapshot_ledger(self):
        helpers = Path(__file__).resolve().parent
        preimage = helpers.parents[1] / "target" / "claude-capture" / "snapshot" / "preimage-20261019"
        if not (preimage / "manifest.json").is_file():
            self.skipTest("local genuine pre-edit helper receipt not present; no prior-artifact claim")
        manifest = json.loads((preimage / "manifest.json").read_bytes())
        expected = {"recording_jobs.py": "4cd7223247e5f175e07c2454869dda2f67e676f62efaccc5922b094550d5aa8f",
                    "capture_source.py": "db47a9dbbdc1d918161a42d9e5ebded0ae8f7881d1a43fe0be92117c1a6714aa"}
        for name, sha in expected.items():
            self.assertEqual(manifest[name], sha)
            self.assertEqual(hashlib.sha256((preimage / name).read_bytes()).hexdigest(), sha)
        current = jobs._new("s", config=self.claude)
        self.assertEqual(current["schema"], "mneme.claude-recording.state.v2")
        code = r'''
import hashlib, json, pathlib, sys
prior_path, current_helpers = map(pathlib.Path, sys.argv[1:])
sys.path[:0] = [str(prior_path), str(current_helpers)]
import recording_jobs as prior
import capture_source
assert pathlib.Path(prior.__file__).resolve() == (prior_path / "recording_jobs.py").resolve()
assert pathlib.Path(capture_source.__file__).resolve() == (prior_path / "capture_source.py").resolve()
payload = json.loads(sys.stdin.buffer.read())
config = {"schema": "mneme.claude-hooks.config.v1", "source_host": "claude_code", "assessor": "claude_code"}
old = prior._new("s", config=config)
print(json.dumps({"old_schema": old["schema"], "old_accepts_old": bool(prior._valid(old, "s", config)),
                  "old_accepts_new": bool(prior._valid(payload, "s", config)), "old_ledger": old}))
'''
        result = subprocess.run([sys.executable, "-B", "-c", code, str(preimage), str(helpers)],
                                input=encoded(current), capture_output=True, check=True, timeout=10)
        observed = json.loads(result.stdout)
        self.assertEqual(observed["old_schema"], "mneme.claude-recording.state.v1")
        self.assertTrue(observed["old_accepts_old"])
        self.assertFalse(observed["old_accepts_new"])
        self.assertTrue(jobs._valid(current, "s", self.claude))
        self.assertFalse(jobs._valid(observed["old_ledger"], "s", self.claude))

    def test_next_prompt_interrupts_only_unclosed_jobs(self):
        def seed(data):
            data["jobs"] = [{"key": "open", "turn_id": "t0", "phase": "admitted", "admission": {"session_id": "s", "expected_prompt_sha256": "a" * 64, "start_offset": 0}, "source_host": "claude_code", "assessor": "claude_code", "source_start_ref": None},
                            {"key": "closed", "turn_id": "t1", "phase": "observing", "admission": {"session_id": "s", "expected_prompt_sha256": "a" * 64, "start_offset": 0}, "source_host": "claude_code", "assessor": "claude_code", "source_start_ref": None}]
            return None, True
        self.assertTrue(jobs._transaction(self.claude, "s", seed, create=True)[0])
        self.assertTrue(jobs.interrupt_open_turns(self.claude, "s"))
        value = jobs._load(jobs._path(self.claude, "s"), "s", self.claude)
        self.assertEqual([j["key"] for j in value["jobs"]], ["closed"])
        self.assertEqual(value["counts"]["cancelled"], 1)

    def test_cross_host_slots_share_aggregate_capacity_and_release(self):
        for first, second in ((self.codex, self.claude), (self.claude, self.codex)):
            with self.subTest(first=first.get("source_host", "codex")):
                held = [reader._slot(first) for _ in range(reader.MAX_WORKERS)]
                try:
                    self.assertTrue(all(type(fd) is int for fd in held))
                    self.assertIsNone(reader._slot(second))
                    os.close(held.pop())
                    released = reader._slot(second)
                    self.assertIsInstance(released, int)
                    os.close(released)
                finally:
                    for fd in held:
                        os.close(fd)

    def test_worker_entry_and_launcher_select_claude_loader_explicitly(self):
        config_path = self.root / "claude-adapter.json"
        config_path.write_text("{}")
        self.claude["_config_path"] = config_path
        self.claude["selection_cwd"] = self.root
        prepared = {**self.claude, "frozen_selection": {}}
        with patch("claude_config.load_config", return_value=prepared) as load, \
             patch("hooks._config", side_effect=AssertionError("Codex loader not allowed")), patch.object(reader, "serve") as serve:
            self.assertEqual(reader.main(["--serve", "--source-host", "claude_code", "--config", str(config_path),
                                          "--session-id", "s", "--selection-cwd", str(self.root)]), 0)
        load.assert_called_once_with(config_path, cwd=self.root)
        serve.assert_called_once_with(prepared, "s")
        reader.notice(self.claude, {"session_id": "s", "turn_id": "t", "prompt": "work"}, False)
        child = unittest.mock.Mock(pid=12345)
        with patch("recording_jobs.has_work", return_value=True), patch.object(reader, "_alive", return_value=False), \
             patch("host_process.spawn", return_value=child) as spawn, \
             patch("host_process.spawned_identity", return_value={"synthetic": True}):
            self.assertEqual(reader.background(self.claude, {"hook_event_name": "Stop", "session_id": "s", "turn_id": "t"}),
                             {"outcome": "launched"})
        argv = spawn.call_args.args[0]
        self.assertEqual(argv[argv.index("--source-host") + 1], "claude_code")
        self.assertNotIn("reader_codex", self.claude)

    def _claude_open(self):
        self.claude["transcripts_root"] = self.root
        self.transcript = self.root / "synthetic.jsonl"
        self.transcript.write_bytes(b"")
        self.event = {"session_id": "s", "turn_id": "t", "prompt": "Inspect this setting.",
                      "transcript_path": str(self.transcript), "cwd": str(self.root)}
        jobs.session_start(self.claude, {"session_id": "s", "source": "startup"})
        self.assertEqual(jobs.notice(self.claude, self.event), {"outcome": "admitted"})
        reader.notice(self.claude, self.event, True)

    def _append(self, row):
        if "message" in row:
            row["message"]["role"] = row["type"]
        with self.transcript.open("ab") as stream:
            stream.write(encoded(row) + b"\n")

    def _claude_close(self, flush=True, boundary_flush=True):
        if flush:
            self._append({"type": "user", "uuid": "u", "sessionId": "s", "parentUuid": None,
                          "message": {"content": [{"type": "text", "text": self.event["prompt"]}]}})
            self._append({"type": "assistant", "uuid": "a", "sessionId": "s", "parentUuid": "u",
                          "message": {"content": [{"type": "text", "text": "Inspection remains a proposal."}]}})
        receipt = {"hook_event_name": "Stop", "session_id": "s", "prompt_id": "t",
                   "transcript_path": str(self.transcript), "stop_hook_active": False,
                   "last_assistant_message": "Inspection remains a proposal."}
        self.assertTrue(jobs.record_stop(self.claude, "s", "t", receipt))
        boundary_event = {**self.event, "turn_id": "next", "prompt": "Next unrelated prompt."}
        self.assertTrue(jobs.confirm_prompt_boundary(self.claude, boundary_event))
        if flush and boundary_flush:
            self._append_boundary()
        reader.close_turn(self.claude, "s", "t")

    def _append_boundary(self):
        self._append({"type": "user", "uuid": "next-u", "sessionId": "s", "parentUuid": "a",
                      "message": {"content": "Next unrelated prompt."}})

    @contextmanager
    def _native_reads(self):
        identity = {"outcome": "ok", "db_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "native_work": {"decoded_bytes": 100}}
        overlap = {"outcome": "empty", "db_id": identity["db_id"], "cards": [], "native_work": {"decoded_bytes": 200}}
        with patch("hook_recall.resolve_project_identity", return_value=identity), patch("hook_recall.collect_overlap", return_value=overlap):
            yield

    def _step(self, runtime, **kwargs):
        with self._native_reads():
            return jobs.step(self.claude, "s", runtime,
                lambda key: reader._recording_reserve(self.claude, "s", key),
                lambda key, result: reader._recording_account(self.claude, "s", key, result),
                reserve_packet=lambda key, packet: reader._recording_reserve(self.claude, "s", key, packet), **kwargs)

    def _proposal_result(self, observed):
        _, context = contract.prepare(observed)
        self.assertIsInstance(context, contract.ValidationContext, context)
        b = context.bindings[0]
        citation = contract.Citation(b.evidence_id, b.kind, b.source_ref_json, b.source_field, b.rendering)
        return {"provider_attempt": True, "usage": {"input_tokens": 5, "output_tokens": 2}, "reason": "proposed",
                "proposal": contract.Proposal("lesson", "A scoped proposal.",
                    "The source records a proposed inspection; later execution and completion remain unknown.", (citation,), None)}

    def _verified_receipt(self):
        return {"db": "project", "db_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "id": "01ARZ3NDEKTSV4RRFFQ69G5FAW",
                "readback_status": "verified", "replayed": False}

    def _flush_response(self, text="Inspection remains a proposal."):
        self._append({"type": "user", "uuid": "u", "sessionId": "s", "parentUuid": None,
                      "message": {"content": self.event["prompt"]}})
        self._append({"type": "assistant", "uuid": "a", "sessionId": "s", "parentUuid": "u",
                      "message": {"content": text}})
        return {"hook_event_name": "Stop", "session_id": "s", "prompt_id": "t",
                "transcript_path": str(self.transcript), "stop_hook_active": False, "last_assistant_message": text}

    def _end_session(self):
        self.assertTrue(jobs.confirm_session_end(self.claude, {**self.event, "hook_event_name": "SessionEnd"}))
        reader.end_session(self.claude, "s")

    def _next_poll(self):
        jobs._transaction(self.claude, "s", lambda d: (d["jobs"][0].update(next_poll=0), True))

    def _state(self):
        return jobs._load(jobs._path(self.claude, "s"), "s", self.claude)

    def test_claude_preflush_stop_then_late_source_uses_shared_reservation(self):
        self._claude_open()
        self._claude_close(flush=False)
        runtime = unittest.mock.Mock()
        self.assertTrue(self._step(runtime))
        runtime.assess.assert_not_called()
        self.assertEqual(self._state()["jobs"][0]["polls"], 1)
        self._append({"type": "user", "uuid": "u", "sessionId": "s", "parentUuid": None,
                      "message": {"content": self.event["prompt"]}})
        self._append({"type": "assistant", "uuid": "a", "sessionId": "s", "parentUuid": "u",
                      "message": {"content": "Inspection remains a proposal."}})
        self._append_boundary()
        jobs._transaction(self.claude, "s", lambda d: (d["jobs"][0].update(next_poll=0), True))
        runtime.assess.return_value = {"provider_attempt": True, "usage": {"input_tokens": 11, "output_tokens": 3}, "reason": "abstained"}
        self.assertTrue(self._step(runtime))
        runtime.assess.assert_called_once()
        state = self._state()
        self.assertEqual(state["counts"]["abstained"], 1)
        self.assertEqual(state["receipts"][0]["source_host"], "claude_code")
        paid = json.loads(reader._paths(self.claude, "s")[0].read_bytes())
        self.assertEqual((paid["attempts"], paid["input_tokens"], paid["output_tokens"]), (1, 11, 3))
        self.assertIsNone(paid["reservation"])
        self.assertIsNone(paid["pending"])

    def test_claude_config_change_is_unpaid_but_uncertain_provider_stops(self):
        self._claude_open(); self._claude_close()
        self.service.write_text('{"changed":true}')
        runtime = unittest.mock.Mock()
        self.assertTrue(self._step(runtime))
        runtime.assess.assert_not_called()
        self.assertEqual(self._state()["receipts"][0]["reason"], "config_changed")
        paid = json.loads(reader._paths(self.claude, "s")[0].read_bytes())
        self.assertEqual(paid["attempts"], 0)

    def test_claude_unknown_usage_preserves_durable_stop(self):
        self._claude_open(); self._claude_close()
        runtime = unittest.mock.Mock()
        runtime.assess.return_value = {"provider_attempt": True, "usage": None, "reason": "transport_error"}
        self.assertTrue(self._step(runtime))
        state = self._state()
        self.assertEqual(state["jobs"][0]["phase"], "unresolved")
        self.assertTrue(state["usage_unknown"])
        paid = json.loads(reader._paths(self.claude, "s")[0].read_bytes())
        self.assertTrue(paid["unknown_usage"])
        self.assertEqual(paid["attempts"], 1)
        self.assertFalse(self._step(runtime))
        runtime.assess.assert_called_once()

    def test_claude_cancellation_before_stop_never_assesses(self):
        self._claude_open()
        self.assertTrue(jobs.close_turn(self.claude, "s", "t", cancel=True))
        self.assertEqual(self._state()["counts"]["cancelled"], 1)
        runtime = unittest.mock.Mock()
        self.assertFalse(self._step(runtime))
        runtime.assess.assert_not_called()

    def test_long_history_baseline_allows_next_preflush_at_exact_eof(self):
        self.claude["transcripts_root"] = self.root
        self.transcript = self.root / "history.jsonl"
        old = [{"type": "file-history-snapshot", "snapshot": "x" * 4096} for _ in range(225)]
        self.transcript.write_bytes(b"".join(encoded(row) + b"\n" for row in old))
        baseline = self.transcript.stat().st_size
        self.assertGreater(baseline, 900_000)
        self.event = {"session_id": "s", "turn_id": "baseline", "prompt": "Resume current work.",
                      "transcript_path": str(self.transcript), "cwd": str(self.root)}
        jobs.session_start(self.claude, {"session_id": "s", "source": "resume"})
        self.assertEqual(jobs.notice(self.claude, self.event), {"outcome": "baseline_only"})
        self.assertEqual(self._state()["watermark"], baseline)
        self.assertEqual(jobs.notice(self.claude, self.event), {"outcome": "replay"})
        self.event.update(turn_id="t", prompt="Inspect this new setting.", agent_type="custom-root-agent")
        self.assertEqual(jobs.notice(self.claude, self.event), {"outcome": "admitted"})
        admitted = self._state()["jobs"][0]
        self.assertEqual(admitted["admission"]["start_offset"], baseline)
        reader.notice(self.claude, self.event, True)
        self._claude_close()
        runtime = unittest.mock.Mock()
        runtime.assess.return_value = {"provider_attempt": True, "usage": {"input_tokens": 4, "output_tokens": 1}, "reason": "abstained"}
        self.assertTrue(self._step(runtime))
        runtime.assess.assert_called_once()
        observed = runtime.assess.call_args.args[0]
        self.assertLess(observed["work"]["source_bytes_read"], 4096)
        self.assertEqual(observed["evidence"][0]["ref"]["byte_offset"], baseline)
        self.assertEqual(observed["evidence"][0]["ref"]["ordinal_scope"], "source_window")
        self.assertEqual(self._state()["counts"]["abstained"], 1)

    def test_claude_native_payload_uses_host_source_without_codex_machinery(self):
        self._claude_open(); self._claude_close()
        runtime = unittest.mock.Mock()
        def assess(observed, cards, **kwargs):
            _, context = contract.prepare(observed)
            b = context.bindings[0]
            citation = contract.Citation(b.evidence_id, b.kind, b.source_ref_json, b.source_field, b.rendering)
            return {"provider_attempt": True, "usage": {"input_tokens": 5, "output_tokens": 2}, "reason": "proposed",
                    "proposal": contract.Proposal("lesson", "A scoped proposal.", "The assistant proposed inspection; execution remains unknown.", (citation,), None)}
        runtime.assess.side_effect = assess
        writes = []
        def write(config, job, timeout):
            writes.append(job)
            return {"db": "project", "db_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "id": "01ARZ3NDEKTSV4RRFFQ69G5FAW",
                    "readback_status": "verified", "replayed": False}
        self.assertTrue(self._step(runtime, native_write=write))
        self.assertEqual(len(writes), 1)
        self.assertEqual(writes[0]["payload"]["source"]["namespace"], "claude-code-acquisition.v1")
        self.assertEqual(writes[0]["payload"]["source"]["reference"],
                         "claude-code://s/t?snapshot=" + writes[0]["source_snapshot"]["observed_sha256"])
        self.assertNotIn("reader_codex", self.claude)
        self.assertEqual(self._state()["counts"]["verified"], 1)

    def test_unpaid_transport_failure_is_not_uncertain_usage(self):
        self._claude_open(); self._claude_close()
        runtime = unittest.mock.Mock()
        runtime.assess.return_value = {"provider_attempt": False, "usage": None, "reason": "invalid_config"}
        self.assertTrue(self._step(runtime))
        self.assertFalse(self._state()["usage_unknown"])
        paid = json.loads(reader._paths(self.claude, "s")[0].read_bytes())
        self.assertFalse(paid["unknown_usage"])
        self.assertIsNone(paid["reservation"])
        self.assertEqual((paid["input_tokens"], paid["output_tokens"]), (0, 0))

    def test_duplicate_or_stale_prompt_cannot_interrupt_current_admission(self):
        self._claude_open()
        before = jobs._path(self.claude, "s").read_bytes()
        self.assertEqual(jobs.notice(self.claude, self.event), {"outcome": "replay"})
        self.assertEqual(jobs._path(self.claude, "s").read_bytes(), before)
        stale = {**self.event, "turn_id": "old", "prompt": "An older prompt."}
        self.assertEqual(jobs.notice(self.claude, stale), {"outcome": "replay"})
        self.assertEqual(jobs._path(self.claude, "s").read_bytes(), before)
        self._append({"type": "user", "uuid": "u0", "sessionId": "s", "parentUuid": None,
                      "message": {"content": self.event["prompt"]}})
        fresh = {**self.event, "turn_id": "new", "prompt": "A genuinely new prompt."}
        self.assertEqual(jobs.notice(self.claude, fresh), {"outcome": "admitted"})
        state = self._state()
        self.assertEqual([j["turn_id"] for j in state["jobs"]], ["new"])
        self.assertEqual(state["counts"]["cancelled"], 1)
        unchanged = jobs._path(self.claude, "s").read_bytes()
        self.assertNotEqual(jobs.notice(self.claude, self.event)["outcome"], "admitted")
        self.assertEqual(jobs._path(self.claude, "s").read_bytes(), unchanged)

    def test_targeted_stale_stopfailure_cannot_cancel_another_turn(self):
        self._claude_open()
        job = self._state()["jobs"][0]
        before = jobs._path(self.claude, "s").read_bytes()
        self.assertFalse(jobs._cancelled(self.claude, "s", job))
        self.assertTrue(jobs.close_turn(self.claude, "s", "stale", cancel=True))
        self.assertEqual(jobs._path(self.claude, "s").read_bytes(), before)
        self.assertFalse(jobs._cancelled(self.claude, "s", job))
        self.assertFalse(jobs._cancel_path(self.claude, "s", "stale").exists())

    def test_two_targeted_cancellations_survive_main_lock_contention(self):
        import host_files
        self._claude_open()
        def add_second(data):
            second = copy.deepcopy(data["jobs"][0]); second["turn_id"] = "second"; second["key"] = "b" * 64
            data["jobs"].append(second)
            return None, True
        jobs._transaction(self.claude, "s", add_second)
        initial = self._state()["jobs"]
        self.assertTrue(all(not jobs._cancelled(self.claude, "s", job) for job in initial))
        lock = jobs._directory(self.claude) / ".lock"
        with lock.open("r+") as stream:
            host_files.lock_file(stream, blocking=False)
            with patch.object(jobs, "LIFECYCLE_LOCK_SECONDS", .01):
                self.assertFalse(jobs.close_turn(self.claude, "s", "t", cancel=True))
                self.assertTrue(jobs._cancelled(self.claude, "s", initial[0]))
                self.assertFalse(jobs._cancelled(self.claude, "s", initial[1]))
                self.assertFalse(jobs.close_turn(self.claude, "s", "second", cancel=True))
        self.assertTrue(all(jobs._cancelled(self.claude, "s", job) for job in initial))
        self.assertTrue(jobs.close_turn(self.claude, "s", "t", cancel=True))
        self.assertEqual([j["turn_id"] for j in self._state()["jobs"]], ["second"])
        self.assertTrue(jobs._cancelled(self.claude, "s", self._state()["jobs"][0]))
        self.assertFalse(jobs._cancel_path(self.claude, "s", "t").exists())
        self.assertTrue(jobs._cancel_path(self.claude, "s", "second").exists())

    def test_reader_duplicate_keeps_optional_context_fence(self):
        self._claude_open()
        context = reader._context_token(self.claude, "s")
        fence = reader._fence(self.claude, "s")
        self.assertEqual(reader.notice(self.claude, self.event, True), {"outcome": "replay"})
        self.assertEqual(reader._context_token(self.claude, "s"), context)
        self.assertEqual(reader._fence(self.claude, "s"), fence)

    def test_stop_continuation_and_duplicates_never_spend_until_next_prompt(self):
        self._claude_open()
        self._append({"type": "user", "uuid": "u", "sessionId": "s", "parentUuid": None,
                      "message": {"content": self.event["prompt"]}})
        self._append({"type": "assistant", "uuid": "a", "sessionId": "s", "parentUuid": "u",
                      "message": {"content": "First response segment."}})
        receipt = {"hook_event_name": "Stop", "session_id": "s", "prompt_id": "t",
                   "transcript_path": str(self.transcript), "stop_hook_active": False,
                   "last_assistant_message": "First response segment."}
        self.assertTrue(jobs.record_stop(self.claude, "s", "t", receipt))
        before = jobs._path(self.claude, "s").read_bytes()
        self.assertTrue(jobs.record_stop(self.claude, "s", "t", receipt))
        self.assertEqual(jobs._path(self.claude, "s").read_bytes(), before)
        runtime = unittest.mock.Mock()
        self.assertFalse(self._step(runtime))
        runtime.assess.assert_not_called()
        self._append({"type": "assistant", "uuid": "b", "sessionId": "s", "parentUuid": "a",
                      "message": {"content": "Continued response segment."}})
        newer = {**receipt, "last_assistant_message": "Continued response segment.", "stop_hook_active": True}
        self.assertTrue(jobs.record_stop(self.claude, "s", "t", newer))
        self.assertFalse(self._step(runtime))
        self.assertNotIn("deadline", self._state()["jobs"][0])
        self._append({"type": "user", "uuid": "v", "sessionId": "s", "parentUuid": "b",
                      "message": {"content": "A later actual prompt."}})
        event = {**self.event, "turn_id": "later", "prompt": "A later actual prompt."}
        self.assertTrue(jobs.confirm_prompt_boundary(self.claude, event))
        confirmed = copy.deepcopy(self._state()["jobs"][0])
        self.assertEqual(confirmed["stop_receipt"]["last_assistant_sha256"], hashlib.sha256(newer["last_assistant_message"].encode()).hexdigest())
        self.assertTrue(jobs.record_stop(self.claude, "s", "t", receipt))
        self.assertTrue(jobs.confirm_prompt_boundary(self.claude, event))
        self.assertEqual(self._state()["jobs"][0], confirmed)
        runtime.assess.return_value = {"provider_attempt": True, "usage": {"input_tokens": 9, "output_tokens": 1}, "reason": "abstained"}
        self.assertTrue(self._step(runtime))
        runtime.assess.assert_called_once()
        paid = json.loads(reader._paths(self.claude, "s")[0].read_bytes())
        self.assertEqual(paid["attempts"], 1)

    def test_provisional_capture_is_not_ready_background_work(self):
        self._claude_open()
        self.assertFalse(jobs.has_work(self.claude, "s"))
        self.assertTrue(jobs.has_work(self.claude, "s", ready_only=False))
        self.assertFalse(jobs.has_work(self.claude, "s", ready_only=True))
        self._claude_close(boundary_flush=False)
        self.assertTrue(jobs.has_work(self.claude, "s", ready_only=True))

    def test_flushed_old_turn_waits_for_next_prompt_flush_within_same_budget(self):
        self._claude_open(); self._claude_close(boundary_flush=False)
        runtime = unittest.mock.Mock()
        self.assertTrue(self._step(runtime))
        runtime.assess.assert_not_called()
        provisional = self._state()["jobs"][0]
        self.assertEqual(provisional["phase"], "observing")
        self.assertEqual(provisional["polls"], 1)
        self._append_boundary()
        jobs._transaction(self.claude, "s", lambda d: (d["jobs"][0].update(next_poll=0), True))
        runtime.assess.return_value = {"provider_attempt": True, "usage": {"input_tokens": 6, "output_tokens": 1}, "reason": "abstained"}
        self.assertTrue(self._step(runtime))
        runtime.assess.assert_called_once()
        self.assertEqual(self._state()["counts"]["abstained"], 1)

    def test_session_end_records_observed_snapshot_once_through_shared_worker(self):
        self._claude_open()
        receipt = self._flush_response()
        self.assertTrue(jobs.record_stop(self.claude, "s", "t", receipt))
        self._end_session()
        self.assertTrue(self._state()["ended"])
        runtime = unittest.mock.Mock()
        runtime.assess.side_effect = lambda observed, *_args, **_kwargs: self._proposal_result(observed)
        writes = []
        def write(_config, job, _timeout):
            writes.append(copy.deepcopy(job))
            return self._verified_receipt()
        with self._native_reads(), patch.object(jobs, "_native_write", side_effect=write):
            reader.serve(self.claude, "s", runtime_factory=lambda *_: runtime, idle_seconds=.3)
        runtime.assess.assert_called_once()
        self.assertEqual(len(writes), 1)
        observed = runtime.assess.call_args.args[0]
        self.assertEqual((observed["status"], observed["reason"]), ("observed", "source_snapshot_ready"))
        self.assertEqual(observed["coverage"]["source_turn"], "observed_prefix")
        self.assertEqual(observed["coverage"]["turn_finality"], "unproven")
        self.assertEqual(observed["coverage"]["remaining_transcript"], "unknown")
        self.assertEqual(observed["coverage"]["snapshot"]["trigger"], "session_end")
        state = self._state()
        self.assertFalse(state["jobs"])
        self.assertEqual(state["counts"]["verified"], 1)
        paid = json.loads(reader._paths(self.claude, "s")[0].read_bytes())
        self.assertEqual((paid["attempts"], paid["input_tokens"], paid["output_tokens"]), (1, 5, 2))
        self._end_session()
        self.assertFalse(self._step(runtime, native_write=write))
        self.assertEqual(len(writes), 1)
        runtime.assess.assert_called_once()

    def test_session_end_freezes_endpoint_before_assessment_and_append_does_not_change_intent(self):
        self._claude_open()
        receipt = self._flush_response()
        raw_prefix = self.transcript.read_bytes()
        self.assertTrue(jobs.record_stop(self.claude, "s", "t", receipt))
        self._end_session()
        runtime = unittest.mock.Mock()
        frozen = []
        def assess(observed, *_args, **_kwargs):
            snapshot = copy.deepcopy(observed["coverage"]["snapshot"])
            intent = self._state()["jobs"][0]
            self.assertEqual(intent["phase"], "assessing")
            self.assertEqual(intent["source_snapshot"], snapshot)
            self.assertEqual(snapshot["observed_sha256"], hashlib.sha256(raw_prefix).hexdigest())
            endpoint = snapshot["observed_through"]
            self.assertEqual(endpoint["byte_offset"] + endpoint["line_bytes"], len(raw_prefix))
            self.assertEqual(endpoint["raw_line_sha256"], hashlib.sha256(raw_prefix.splitlines(keepends=True)[-1]).hexdigest())
            self.assertEqual(snapshot["latest_stop"], "matched")
            self.assertFalse(snapshot["trailing_partial_line"])
            frozen.append(snapshot)
            self._append({"type": "assistant", "uuid": "after-paid", "sessionId": "s", "parentUuid": "a",
                          "message": {"content": "Later source bytes must not enter the frozen paid request."}})
            return self._proposal_result(observed)
        runtime.assess.side_effect = assess
        writes = []
        def write(_config, job, _timeout):
            self.assertEqual(job["source_snapshot"], frozen[0])
            self.assertEqual(self._state()["jobs"][0]["source_snapshot"], frozen[0])
            self.assertEqual(self._state()["jobs"][0]["phase"], "write_intent")
            writes.append(copy.deepcopy(job["payload"]))
            self.assertNotIn("Later source bytes", json.dumps(job["payload"]))
            return self._verified_receipt()
        self.assertTrue(self._step(runtime, native_write=write))
        self.assertEqual(len(writes), 1)
        self.assertEqual(self._state()["receipts"][0]["source_snapshot"], frozen[0])
        self._end_session()
        self.assertFalse(self._step(runtime, native_write=write))
        self.assertEqual(len(writes), 1)
        runtime.assess.assert_called_once()

    def test_session_end_waits_for_late_final_before_last_poll(self):
        self._claude_open()
        self._append({"type": "user", "uuid": "u", "sessionId": "s", "parentUuid": None,
                      "message": {"content": self.event["prompt"]}})
        receipt = {"hook_event_name": "Stop", "session_id": "s", "prompt_id": "t",
                   "transcript_path": str(self.transcript), "stop_hook_active": False,
                   "last_assistant_message": "A final response flushed later."}
        self.assertTrue(jobs.record_stop(self.claude, "s", "t", receipt))
        self._end_session()
        runtime = unittest.mock.Mock()
        self.assertTrue(self._step(runtime))
        runtime.assess.assert_not_called()
        self.assertEqual(self._state()["jobs"][0]["polls"], 1)
        self._append({"type": "assistant", "uuid": "a", "sessionId": "s", "parentUuid": "u",
                      "message": {"content": receipt["last_assistant_message"]}})
        self._next_poll()
        runtime.assess.return_value = {"provider_attempt": True, "usage": {"input_tokens": 3, "output_tokens": 1}, "reason": "abstained"}
        self.assertTrue(self._step(runtime))
        runtime.assess.assert_called_once()
        observed = runtime.assess.call_args.args[0]
        self.assertEqual(observed["coverage"]["snapshot"]["latest_stop"], "matched")
        self.assertEqual(self._state()["counts"]["abstained"], 1)

    def test_last_poll_captures_prefix_and_omits_unmatched_tool_exchange(self):
        self._claude_open()
        self._append({"type": "user", "uuid": "u", "sessionId": "s", "parentUuid": None,
                      "message": {"content": self.event["prompt"]}})
        self._append({"type": "assistant", "uuid": "a", "sessionId": "s", "parentUuid": "u",
                      "message": {"content": [{"type": "text", "text": "I started inspecting the setting."},
                          {"type": "tool_use", "id": "unfinished", "name": "Read", "input": {"path": "setting"}}]}})
        complete_prefix = self.transcript.read_bytes()
        with self.transcript.open("ab") as stream:
            stream.write(b'{"type":"user","uuid":"partial-tool-result"')
        receipt = {"hook_event_name": "Stop", "session_id": "s", "prompt_id": "t",
                   "transcript_path": str(self.transcript), "stop_hook_active": False,
                   "last_assistant_message": "An unobserved final response."}
        self.assertTrue(jobs.record_stop(self.claude, "s", "t", receipt))
        self._end_session()
        runtime = unittest.mock.Mock()
        runtime.assess.return_value = {"provider_attempt": True, "usage": {"input_tokens": 3, "output_tokens": 1}, "reason": "abstained"}
        for expected_poll in range(1, jobs.MAX_POLLS + 1):
            with self.subTest(poll=expected_poll):
                self.assertTrue(self._step(runtime))
                if expected_poll < jobs.MAX_POLLS:
                    runtime.assess.assert_not_called()
                    self.assertEqual(self._state()["jobs"][0]["polls"], expected_poll)
                    self._next_poll()
        runtime.assess.assert_called_once()
        observed = runtime.assess.call_args.args[0]
        coverage = observed["coverage"]
        snapshot = coverage["snapshot"]
        self.assertEqual(snapshot["trigger"], "session_end")
        self.assertEqual(snapshot["latest_stop"], "not_observed")
        self.assertTrue(snapshot["trailing_partial_line"])
        self.assertEqual(snapshot["unmatched_calls"], 1)
        self.assertEqual(snapshot["observed_sha256"], hashlib.sha256(complete_prefix).hexdigest())
        self.assertEqual(snapshot["observed_through"]["byte_offset"] + snapshot["observed_through"]["line_bytes"], len(complete_prefix))
        self.assertEqual(coverage["missing_results"], 0)
        self.assertGreaterEqual(coverage["omissions"]["incomplete_tool_exchange"], 1)
        self.assertFalse(any(item["kind"] in ("tool_call", "tool_result") for item in observed["evidence"]))
        self.assertEqual(coverage["turn_finality"], "unproven")
        self.assertFalse(self._state()["jobs"])
        self.assertFalse(self._step(runtime))
        runtime.assess.assert_called_once()

    def test_legitimate_resume_observes_retained_stopped_prompt_once(self):
        self._claude_open()
        receipt = self._flush_response()
        self.assertTrue(jobs.record_stop(self.claude, "s", "t", receipt))
        resume = {**self.event, "hook_event_name": "SessionStart", "source": "resume", "turn_id": None}
        self.assertTrue(jobs.recover_stopped_turns(self.claude, resume))
        armed = copy.deepcopy(self._state()["jobs"][0])
        self.assertEqual(armed["phase"], "observing")
        self.assertEqual(armed["boundary_receipt"]["kind"], "resume")
        self.assertTrue(jobs.recover_stopped_turns(self.claude, resume))
        self.assertEqual(self._state()["jobs"][0], armed)
        runtime = unittest.mock.Mock()
        runtime.assess.return_value = {"provider_attempt": True, "usage": {"input_tokens": 3, "output_tokens": 1}, "reason": "abstained"}
        self.assertTrue(self._step(runtime))
        runtime.assess.assert_called_once()
        observed = runtime.assess.call_args.args[0]
        self.assertEqual(observed["coverage"]["snapshot"]["trigger"], "resume")
        self.assertEqual(observed["coverage"]["turn_finality"], "unproven")
        self.assertTrue(jobs.recover_stopped_turns(self.claude, resume))
        self.assertFalse(self._step(runtime))
        runtime.assess.assert_called_once()
        paid = json.loads(reader._paths(self.claude, "s")[0].read_bytes())
        self.assertEqual(paid["attempts"], 1)

    def test_crash_after_assessment_reservation_retains_snapshot_without_reassessment(self):
        class SimulatedCrash(BaseException):
            pass
        self._claude_open()
        receipt = self._flush_response()
        self.assertTrue(jobs.record_stop(self.claude, "s", "t", receipt))
        self._end_session()
        runtime = unittest.mock.Mock()
        def crash(observed, *_args, **_kwargs):
            self.assertEqual(self._state()["jobs"][0]["source_snapshot"], observed["coverage"]["snapshot"])
            raise SimulatedCrash()
        runtime.assess.side_effect = crash
        with self.assertRaises(SimulatedCrash):
            self._step(runtime)
        before = self._state()["jobs"][0]
        self.assertEqual(before["phase"], "assessing")
        self.assertTrue(jobs.recover_stopped_turns(self.claude,
            {**self.event, "hook_event_name": "SessionStart", "source": "resume", "turn_id": None}))
        self.assertEqual(self._state()["jobs"][0], before)
        self._append({"type": "assistant", "uuid": "later", "sessionId": "s", "parentUuid": "a",
                      "message": {"content": "Do not reassess this later content."}})
        self.assertTrue(self._step(runtime))
        state = self._state()
        self.assertEqual(state["jobs"][0]["phase"], "unresolved")
        self.assertEqual(state["jobs"][0]["source_snapshot"], before["source_snapshot"])
        self.assertTrue(state["usage_unknown"])
        self.assertFalse(self._step(runtime))
        runtime.assess.assert_called_once()

    def test_lost_native_ack_keeps_frozen_snapshot_and_never_replays_write(self):
        class SimulatedLostAck(BaseException):
            pass
        self._claude_open()
        receipt = self._flush_response()
        self.assertTrue(jobs.record_stop(self.claude, "s", "t", receipt))
        self._end_session()
        runtime = unittest.mock.Mock()
        runtime.assess.side_effect = lambda observed, *_args, **_kwargs: self._proposal_result(observed)
        writes = []
        def lost_ack(_config, job, _timeout):
            writes.append(copy.deepcopy(job))
            self.assertEqual(self._state()["jobs"][0]["phase"], "write_intent")
            raise SimulatedLostAck()
        with self.assertRaises(SimulatedLostAck):
            self._step(runtime, native_write=lost_ack)
        before = self._state()["jobs"][0]
        self._append({"type": "assistant", "uuid": "later", "sessionId": "s", "parentUuid": "a",
                      "message": {"content": "Later bytes cannot change an uncertain native intent."}})
        self.assertTrue(self._step(runtime, native_write=lost_ack))
        state = self._state()
        self.assertEqual(state["jobs"][0]["phase"], "unresolved")
        self.assertEqual(state["jobs"][0]["source_snapshot"], before["source_snapshot"])
        self.assertEqual(state["jobs"][0]["payload"], before["payload"])
        self.assertFalse(self._step(runtime, native_write=lost_ack))
        self.assertEqual(len(writes), 1)
        runtime.assess.assert_called_once()
        paid = json.loads(reader._paths(self.claude, "s")[0].read_bytes())
        self.assertEqual((paid["attempts"], paid["input_tokens"], paid["output_tokens"]), (1, 5, 2))
        self.assertFalse(paid["unknown_usage"])


if __name__ == "__main__":
    unittest.main()

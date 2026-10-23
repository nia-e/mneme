"""Prospective automatic-path receipts; synthetic state, never native stores/providers."""
from __future__ import annotations

import json
import os
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import patch

import host_files
import reader_worker as reader
import recording_jobs as jobs
import session_activity
import fixture_source_turn as source
from fixture_recording import DB_ID, OTHER_DB, SESSION, FakeRuntime, RecordingFixture
from reader_test_support import PublicationHandoff

ORIGIN = {"turn": "turn-1", "generation": "a" * 32, "context_generation": 7,
          "attempt": "b" * 64 + ":2"}
CARD = {"id": OTHER_DB, "summary": "PRIVATE CARD BODY", "status": "active", "source": "test",
        "fingerprint": "a" * 64, "db_id": DB_ID}


class ReaderActivityTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.config = {"state_dir": self.root / "state", "service_config": self.root / "service.json",
                       "project_root": self.root, "reader_model": "gpt-6.1-sol", "librarian_effort": "medium"}
        self.config["service_config"].write_text("{}")
        self.event = {"session_id": "activity-session", "turn_id": "turn-1", "cwd": str(self.root),
                      "prompt": "PRIVATE PROMPT MUST NEVER BE PROJECTED"}
        self.events = []
        self.real_emit = session_activity.emit
        mocked = patch("session_activity.emit", side_effect=self.observe)
        mocked.start()
        self.addCleanup(mocked.stop)

    def observe(self, event, kind, outcome, **projection):
        _, lock = reader._paths(self.config, self.event["session_id"])
        if lock.exists():
            with lock.open("r+b") as stream:
                host_files.lock_file(stream, blocking=False)
                host_files.unlock_file(stream)
        self.events.append((dict(event), kind, outcome, projection))
        return True

    def ready(self, *, cards=None):
        self.assertEqual(reader.notice(self.config, self.event, True), {"outcome": "queued"})
        def publish(data):
            self.expected_origin = reader._activity_origin(data, data["pending"])
            data["ready"] = {**data["pending"], "cards": cards or [dict(CARD)], "db_id": DB_ID,
                             "fence": data["fence"], "concerns": []}
            data["pending"] = None
            return None, True
        self.assertTrue(reader._state(self.config, self.event["session_id"], publish)[0])

    def test_notice_reasons_and_logging_failure_do_not_change_admission(self):
        self.assertEqual(reader.notice(self.config, self.event, False)["outcome"], "skipped")
        self.assertEqual(self.events[-1][3]["reason"], "non_substantive")
        second = {**self.event, "turn_id": "turn-2"}
        reader._state(self.config, self.event["session_id"], lambda d: (d.update(unknown_usage=True), True))
        self.assertEqual(reader.notice(self.config, second, True)["outcome"], "budget")
        self.assertEqual(self.events[-1][3]["reason"], "unknown_accounting")
        with patch("session_activity.emit", side_effect=RuntimeError("PRIVATE PROVIDER FAILURE")):
            third = {**self.event, "turn_id": "turn-3"}
            self.assertEqual(reader.notice(self.config, third, True)["outcome"], "budget")

    def test_consumption_is_not_output_and_keeps_exact_origin(self):
        self.ready()
        response = reader.consume(self.config, self.event)
        self.assertEqual(response["outcome"], "emitted")
        self.assertEqual(response["_activity"]["origin"], self.expected_origin)
        self.assertIsNone(response["_activity"]["selection_digest"])
        consumed = [item for item in self.events if item[1] == "delivery_consumed"]
        self.assertEqual(len(consumed), 1)
        self.assertEqual(consumed[0][2], "consumed")
        self.assertEqual(consumed[0][3]["nodes"], [OTHER_DB])
        self.assertFalse(any(item[1] in ("hook_output", "host_admission") for item in self.events))
        self.assertNotIn(CARD["summary"], json.dumps(consumed))
        self.assertNotIn(self.event["prompt"], json.dumps(consumed))

    def test_consumption_control_bytes_identical_when_telemetry_fails(self):
        self.ready()
        path, _ = reader._paths(self.config, self.event["session_id"])
        before = path.read_bytes()
        with patch.object(reader.time, "time", return_value=1000), patch("session_activity.emit", return_value=False):
            disabled = reader.consume(self.config, self.event)
        disabled_bytes = path.read_bytes()
        host_files.atomic_write(path, before)
        with patch.object(reader.time, "time", return_value=1000):
            enabled = reader.consume(self.config, self.event)
        self.assertEqual(disabled, enabled)
        self.assertEqual(path.read_bytes(), disabled_bytes)
        self.assertNotIn(b"activity", disabled_bytes)

    def test_stale_generation_receipt_never_borrows_current_generation(self):
        self.ready()
        reader._invalidate(self.config, self.event["session_id"])
        response = reader.consume(self.config, self.event)
        self.assertEqual(response["outcome"], "stale")
        consumed = [item for item in self.events if item[1] == "delivery_consumed"]
        self.assertEqual(consumed[-1][2], "stale")
        self.assertIsNone(consumed[-1][3]["origin"])
        self.assertNotIn("_activity", response)
        self.assertEqual(consumed[-1][3]["reason"], "stale_context")

    def test_owner_qualification_and_node_overflow_are_explicit(self):
        projection = reader.activity_projection(self.config, ORIGIN,
            [dict(CARD)] * 257 + [{**CARD, "db_id": OTHER_DB}])
        self.assertEqual(len(projection["groups"]), 2)
        first, second = projection["groups"]
        self.assertEqual(first["owner"]["id"], DB_ID)
        self.assertEqual((first["count"], len(first["nodes"]), first["omitted"]), (257, 256, 1))
        self.assertEqual(second["owner"]["id"], OTHER_DB)
        self.assertNotIn("PRIVATE", json.dumps(projection))

    def test_producer_projection_is_accepted_by_real_sidecar_writer(self):
        activity = self.root / "activity"
        activity.mkdir(mode=0o700)
        manifest = self.root / "activity.json"
        host_files.atomic_write(manifest, json.dumps({"schema": "mneme.session-activity.config.v1", "root": str(activity.resolve()),
            "workspaces": [{"path": str(self.root.resolve()), "source": "codex"}], "device": None}).encode())
        with patch.dict(os.environ, {"MNEME_ACTIVITY_CONFIG": str(manifest)}), patch("session_activity.emit", side_effect=self.real_emit):
            self.ready()
            result = reader.consume(self.config, self.event)
        self.assertEqual(result["outcome"], "emitted")
        raw = (activity / session_activity.filename(self.event["session_id"])).read_text()
        record = json.loads(raw)
        self.assertEqual({event["kind"] for event in record["events"]}, {"recall_admission", "delivery_consumed"})
        self.assertEqual(record["events"][-1]["origin"], self.expected_origin)
        self.assertNotIn("PRIVATE", raw)

    def test_background_workspace_uses_only_existing_exact_selection_binding(self):
        self.assertIsNone(reader._activity_event(self.config, "session")["cwd"])
        config = {**self.config, "selection_cwd": str(self.root), "frozen_selection": {"cwd": str(self.root)}}
        self.assertEqual(reader._activity_event(config, "session")["cwd"], str(self.root))
        config["selection_cwd"] = str(self.root / "changed")
        self.assertIsNone(reader._activity_event(config, "session")["cwd"])

    def test_worker_capacity_records_actual_pending_attempt(self):
        reader.notice(self.config, self.event, True)
        with patch.object(reader, "_slot", return_value=None), \
             patch.object(reader, "_activity_event", return_value={key: self.event[key] for key in ("session_id", "turn_id", "cwd")}):
            reader.serve(self.config, self.event["session_id"], idle_seconds=.01)
        capacity = [event for event in self.events if event[2] == "capacity"]
        self.assertEqual(len(capacity), 1)
        self.assertEqual(capacity[0][3]["origin"]["turn"], "turn-1")
        self.assertEqual(capacity[0][3]["reason"], "worker_capacity")
        self.assertIsNotNone(capacity[0][3]["origin"]["generation"])

    def test_real_selector_publication_carries_attempt_into_consumption(self):
        class Runtime:
            def __init__(self, *_args): pass
            def close(self): pass
        attempts = []
        def execute(_config, _cue, _runtime, reserve, _emitted, **_kwargs):
            attempts.append(reserve())
            return {"outcome": "selected", "cards": [dict(CARD)], "db_id": DB_ID,
                    "provider_attempt": True, "usage": {"input_tokens": 5, "output_tokens": 1}}
        reader.notice(self.config, self.event, True)
        with patch.object(reader, "_execute", side_effect=execute), \
             patch.object(reader, "_activity_event", return_value={key: self.event[key] for key in ("session_id", "turn_id", "cwd")}), \
             PublicationHandoff(reader, self.event["session_id"]) as handoff:
            worker = threading.Thread(target=reader.serve, args=(self.config, self.event["session_id"]),
                                      kwargs={"runtime_factory": Runtime, "idle_seconds": .15})
            worker.start()
            try:
                handoff.wait()
                consumed = reader.consume(self.config, self.event)
                reader.close_turn(self.config, self.event["session_id"], self.event["turn_id"])
            finally:
                handoff.finish()
                worker.join(3)
        self.assertFalse(worker.is_alive())
        self.assertEqual(attempts, [True])
        selected = [item for item in self.events if item[1] == "recall_selection"]
        self.assertEqual(len(selected), 1)
        self.assertEqual(selected[0][2], "selected")
        self.assertEqual(selected[0][3]["origin"], consumed["_activity"]["origin"])
        self.assertNotIn("PRIVATE", json.dumps(selected))


class RecordingActivityTests(unittest.TestCase, RecordingFixture):
    def setUp(self):
        RecordingFixture.__init__(self, self)
        self.events = []
        mocked = patch("session_activity.emit", side_effect=self.observe_activity)
        mocked.start()
        self.addCleanup(mocked.stop)

    def observe_activity(self, event, kind, outcome, **projection):
        lock = jobs._directory(self.config) / ".lock"
        with lock.open("r+b") as stream:
            host_files.lock_file(stream, blocking=False)
            host_files.unlock_file(stream)
        self.events.append((dict(event), kind, outcome, projection))
        return True

    def test_retained_save_and_maintenance_are_distinct_body_free_receipts(self):
        job = self.begin()
        receipt = {"db": "project", "db_id": DB_ID, "id": OTHER_DB, "readback_status": "verified"}
        def finish(data):
            current = data["jobs"][0]
            current.update(db_id=DB_ID, proposal_kind="lesson", maintenance=[{"target": OTHER_DB, "status": "unchanged"}],
                           save_outcome={"status": "verified", "native": receipt})
            jobs._finish(data, current, "verified", "native_verified", receipt)
            return None, True
        self.assertTrue(jobs._transaction(self.config, SESSION, finish)[0])
        saves = [event for event in self.events if event[2] == "save_confirmed"]
        maintenance = [event for event in self.events if event[2] == "maintenance_verified"]
        self.assertEqual(len(saves), 1)
        self.assertEqual(len(maintenance), 1)
        self.assertEqual(saves[0][3]["nodes"], [OTHER_DB])
        self.assertEqual(saves[0][3]["origin"]["attempt"], job["key"])
        self.assertIsNone(saves[0][3]["origin"]["generation"])
        self.assertNotIn("prompt_sha256", json.dumps(self.events))
        self.assertNotIn("native", self.state()["receipts"][-1]["save_outcome"])
        self.assertNotIn("activity", self.state())

    def test_maintenance_only_verified_job_never_becomes_save(self):
        self.begin()
        def finish(data):
            current = data["jobs"][0]
            current.update(db_id=DB_ID, maintenance=[{"target": OTHER_DB, "status": "applied"}])
            jobs._finish(data, current, "verified", "native_verified")
            return None, True
        self.assertTrue(jobs._transaction(self.config, SESSION, finish)[0])
        self.assertTrue(any(event[2] == "maintenance_verified" for event in self.events))
        self.assertFalse(any(event[2] == "save_confirmed" for event in self.events))

    def test_failed_control_write_does_not_emit_receipt_or_change_job(self):
        self.begin()
        before = self.state()
        with patch.object(jobs, "_write", side_effect=OSError("PRIVATE PATH")):
            ok, _ = jobs._update(self.config, SESSION, before["jobs"][0]["key"],
                                lambda d, j: jobs._finish(d, j, "abstained", "assessment_abstained"))
        self.assertFalse(ok)
        self.assertEqual(self.state(), before)
        self.assertEqual(self.events, [])

    def test_diagnostic_exception_never_unsettles_save(self):
        self.begin()
        receipt = {"db": "project", "db_id": DB_ID, "id": OTHER_DB, "readback_status": "verified"}
        def finish(data):
            current = data["jobs"][0]
            current.update(db_id=DB_ID, proposal_kind="lesson")
            jobs._finish(data, current, "verified", "native_verified", receipt)
            return None, True
        with patch("session_activity.emit", side_effect=RuntimeError("PRIVATE FAILURE")):
            self.assertTrue(jobs._transaction(self.config, SESSION, finish)[0])
        self.assertEqual(self.state()["jobs"], [])
        self.assertEqual(self.state()["receipts"][-1]["native"], receipt)

    def test_normal_observer_records_packet_without_selector_attribution(self):
        job = self.begin()
        # Existing historical workspace is enough for this packet observation;
        # there is deliberately no selector receipt or generation to recover.
        self.mutate(lambda state: state["jobs"][0].update(
            frozen_selection={"cwd": str(self.root.resolve()), "digest": "c" * 64}))
        packet = self.delivery_packet()
        self.assertEqual(jobs.attach_delivery(self.config, packet), {"outcome": "attached"})
        self.assertEqual(self.events, [])
        self.append([source.row("response_item", {
            "type": "message", "id": "hook-packet", "role": "developer",
            "content": [{"type": "input_text", "text": packet["rendered_text"]}],
            source.META: {"turn_id": "turn-1", "content_item_kinds": ["hooks.additional_context"]}})])
        self.source_complete()
        jobs.close_turn(self.config, SESSION, "turn-1")
        with patch.object(session_activity, "_read", side_effect=AssertionError("no selector lookup")):
            self.step(FakeRuntime(self, reason="abstained"))
        admitted = [event for event in self.events if event[1] == "host_admission"]
        self.assertEqual(len(admitted), 1)
        self.assertEqual(admitted[0][2], "admitted_retained")
        self.assertEqual(admitted[0][3]["origin"], {"turn": "turn-1", "generation": None,
            "context_generation": None, "attempt": "recording:" + job["key"]})
        self.assertEqual(admitted[0][3]["reason"], "selector_origin_unavailable")
        self.assertEqual(admitted[0][3]["owner"], {"name": "unknown", "id": DB_ID, "db": "unknown"})
        self.assertEqual(admitted[0][3]["nodes"], [OTHER_DB])
        self.assertNotIn("selection_digest", admitted[0][3])
        self.assertNotIn(packet["rendered_text"], json.dumps(self.events))
        self.assertEqual(self.writes, [])

    def test_missing_delayed_ambiguous_selector_receipts_leave_same_unlinked_observation(self):
        source_job = self.begin()
        job = {**source_job, "delivery": self.delivery_packet(), "frozen_selection": {
            "schema": "mneme.native-store-selection.v1", "cwd": str(self.root.resolve()),
            "snapshot": {"active": [{"name": "research", "store": {
                "id": DB_ID, "owner": {"db": "notes"}}}]}}}
        receipt_file = self.root / "optional-sidecar.json"
        prior_receipts = ([], [{"origin": ORIGIN, "delayed": True}],
                          [{"origin": ORIGIN}, {"origin": {**ORIGIN, "generation": "d" * 32}}])
        projections = []
        for prior in prior_receipts:
            receipt_file.write_text(json.dumps(prior))
            data = jobs._ActivityTransaction({"session_id": SESSION})
            with patch.object(session_activity, "_read", side_effect=AssertionError("no selector lookup")):
                jobs._capture_admission(data, job, {"coverage": {"memory_delivery": "admitted_retained"}})
            self.assertEqual(len(data.activity), 1)
            projections.append(data.activity)
        self.assertEqual(projections[0], projections[1])
        self.assertEqual(projections[1], projections[2])
        projection = projections[0][0][3]
        self.assertEqual(projection["owner"], {"name": "research", "id": DB_ID, "db": "notes"})
        self.assertEqual(projection["nodes"], [OTHER_DB])
        self.assertEqual(projection["reason"], "selector_origin_unavailable")
        self.assertIsNone(projection["origin"]["generation"])
        self.assertIsNone(projection["origin"]["context_generation"])
        self.assertTrue(projection["origin"]["attempt"].startswith("recording:"))

    def test_activity_io_drains_after_whole_recording_step(self):
        # Preparing/enqueueing a safe event performs no binding I/O.
        event = {"session_id": SESSION, "turn_id": "turn-1", "cwd": str(self.root)}
        with patch.object(session_activity, "_append", return_value=True) as append:
            def operation(*_args, **_kwargs):
                metadata, observation = session_activity._prepare(event, "recording", "abstained",
                    reason=None, origin=ORIGIN, owner=None, nodes=[], count=0, omitted=0,
                    selection_digest=None, key=None, at=1000)
                batch = session_activity._DEFERRED.get()
                self.assertIsNotNone(batch)
                batch.enqueue(metadata, observation)
                append.assert_not_called()
                return "functional_result"
            with patch.object(jobs, "_step", side_effect=operation):
                self.assertEqual(jobs.step(self.config, SESSION, None, None, None), "functional_result")
            append.assert_called_once()
        with patch.object(session_activity, "defer", side_effect=RuntimeError("logger broken")), \
             patch.object(jobs, "_step", return_value="still_functional"):
            self.assertEqual(jobs.step(self.config, SESSION, None, None, None), "still_functional")

    def test_activity_attachment_is_byte_identical_at_job_capacity(self):
        self.begin()
        packet = self.delivery_packet()
        def fill(data):
            job = data["jobs"][0]
            job["padding"] = ""
            room = jobs.MAX_JOB_BYTES - len(jobs.encoded({**job, "delivery": packet})) - 1
            job["padding"] = "x" * room
            return None, True
        self.assertTrue(jobs._transaction(self.config, SESSION, fill)[0])
        path = jobs._path(self.config, SESSION)
        before = path.read_bytes()
        self.assertEqual(jobs.attach_delivery(self.config, packet), {"outcome": "attached"})
        without = path.read_bytes()
        host_files.atomic_write(path, before)
        with patch("session_activity.emit", return_value=False):
            result = jobs.attach_delivery(self.config, packet)
        self.assertEqual(result, {"outcome": "attached"})
        self.assertEqual(path.read_bytes(), without)
        self.assertNotIn(b"activity", without)
        self.assertLessEqual(len(jobs.encoded(self.state()["jobs"][0])), jobs.MAX_JOB_BYTES)

    def test_unknown_verified_operation_cannot_claim_save(self):
        self.begin()
        receipt = {"db": "project", "db_id": DB_ID, "id": OTHER_DB, "readback_status": "verified"}
        def finish(data):
            current = data["jobs"][0]
            current["db_id"] = DB_ID
            jobs._finish(data, current, "verified", "native_verified", receipt)
            return None, True
        self.assertTrue(jobs._transaction(self.config, SESSION, finish)[0])
        self.assertFalse(any(event[2] == "save_confirmed" for event in self.events))

    def test_legacy_job_without_historical_workspace_never_joins_on_turn(self):
        self.begin()
        packet = self.delivery_packet()
        self.assertEqual(jobs.attach_delivery(self.config, packet)["outcome"], "attached")
        self.source_complete()
        jobs.close_turn(self.config, SESSION, "turn-1")
        self.step(FakeRuntime(self, reason="abstained"))
        self.assertFalse(any(event[1] == "host_admission" for event in self.events))



if __name__ == "__main__":
    unittest.main()

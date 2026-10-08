import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import memory
from mcp_client import McpProtocolError

ID = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
OTHER_ID = "01ARZ3NDEKTSV4RRFFQ69G5FAW"


def current_context():
    return {"schema": "mneme.context.v7", "core": [], "primary": [],
            "expansions": [], "episodes": [], "touchstone_retrieval": {
                "searched": False, "read_limit": 0, "catalog_reads": 0, "record_reads": 0,
                "referrer_page_reads": 0, "target_reads": 0, "anchors_total": 0,
                "anchors_examined": 0, "owners_discovered": 0,
                "further_tail_unknown": False, "stop_reason": None}}


class FakeClient:
    def __init__(self, catalog=None):
        self.calls = []
        self.catalog = catalog
        self.prepared = None
        self.failure = None

    def prepare_capture(self, value):
        self.calls.append(("capture/prepare", value))
        if self.failure:
            raise self.failure
        self.prepared = value
        return {"id": ID, "digest": "native-digest", "source": value.get("source")}

    def prepare_save(self, value):
        self.calls.append(("save/prepare", value))
        if self.failure:
            raise self.failure
        payload = {"kind": "note", **value}
        payload.setdefault("source", {"namespace": "manual", "key": "frozen-operation",
                                      "reference": "manual-submission:frozen-operation"})
        return {"schema": 1, "id": ID, "payload": payload}

    def save_verified(self, db, value):
        self.calls.append(("save/verified", db, value))
        if self.failure:
            raise self.failure
        return {"kind": value.get("kind", "note"), "id": ID, "replayed": False,
                "readback_status": "verified"}

    def capture_verified(self, db, value):
        self.calls.append(("capture/verified", db, value))
        if self.failure:
            raise self.failure
        return {"id": ID, "replayed": False, "readback_status": "verified"}

    def prepare_episode(self, value):
        self.calls.append(("episode/prepare", value))
        if self.failure:
            raise self.failure
        return {"action": value["action"], "is_mutation": value["action"] in ("append", "revise"),
                "payload": value}

    def episode_verified(self, db, value):
        self.calls.append(("episode/verified", db, value))
        if self.failure:
            raise self.failure
        return {"action": value["action"], "episode_id": ID, "edition_id": OTHER_ID,
                "revision": 1, "replayed": False, "readback_status": "verified"}

    def call_tool(self, name, args):
        self.calls.append((name, args))
        if name == "databases":
            return self.catalog
        if name == "recall_context":
            return current_context()
        if name == "get":
            return {"id": args["id"]}
        if name == "supersede":
            return {"db": args["db"], "ok": True}
        if name == "episode":
            return {"action": args["action"], "items": [], "next": None, "partial": False}
        raise AssertionError(name)


class MemoryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.config = self.root / "service.json"
        self.config.write_text(json.dumps({
            "binary": str(self.root / "mnemed"),
            "project_db": str(self.root / "project.db"), "port": 18765,
            "state_dir": str(self.root / "state"), "working_directory": str(self.root),
        }))
        self.value = {"source": {"namespace": "codex", "key": "session/turn/claim",
                                 "reference": "codex://session/turn"},
                      "summary": "Durable lesson"}
        profile = patch("memory.select_for_cwd", return_value={
            "mode": "default", "library_config": None, "configured": False, "root": None})
        profile.start()
        self.addCleanup(profile.stop)

    def test_read_operations_keep_explicit_database(self):
        client = FakeClient()
        self.assertEqual(memory.recall(client, "task")["schema"], "mneme.context.v7")
        self.assertEqual(memory.get(client, ID)["id"], ID)
        self.assertTrue(memory.supersede(client, ID, OTHER_ID)["ok"])
        self.assertEqual(client.calls[0], ("recall_context", {
            "db": "project", "text": "task", "k": 4, "max_nodes": 8, "depth": 1}))
        self.assertEqual(client.calls[1][1]["db"], "project")

    def test_recall_retains_v6_installed_owner_compatibility_without_old_schema_fallback(self):
        client = FakeClient()
        episode = {"kind": "episode", "id": OTHER_ID, "edition_id": OTHER_ID,
                   "episode_id": ID, "revision": 1, "current_edition_id": OTHER_ID,
                   "occurred": {"kind": "unknown"}, "recorded_at": 10,
                   "edition_recorded_at": 20, "thread": "workshop",
                   "recording_session": "host-specific-recorder-session",
                   "occurrence_contexts": [{"namespace": "conversation", "key": "shared-scene"}],
                   "origins": [{"kind": "lexical"}, {"kind": "reference",
                                "anchor": {"kind": "semantic", "node_id": ID},
                                "from": ID, "to": OTHER_ID, "edge_kind": "Associative",
                                "body_anchor": None}]}
        value = {"schema": "mneme.context.v6", "core": [], "primary": [],
                 "expansions": [], "episodes": [episode],
                 "episodic_retrieval": {"state": "searched", "mode": "lexical"},
                 "episode_reference_retrieval": {"state": "searched", "raw_edges_scanned": 1,
                                                "endpoint_reads": 1, "further_tail_unknown": False}}
        with patch.object(client, "call_tool", return_value=value) as call:
            self.assertIs(memory.recall(client, "workshop timeout", db="user"), value)
            self.assertEqual(call.call_args.args[1]["db"], "user")
        for schema in (None, "mneme.context.v2", "mneme.context.v3", "mneme.context.v4", "mneme.context.v5", "mneme.context.v8"):
            with self.subTest(schema=schema), patch.object(client, "call_tool", return_value={"schema": schema}):
                with self.assertRaisesRegex(memory.BridgeError, "unexpected recall context"):
                    memory.recall(client, "workshop timeout")
        with patch.object(client, "call_tool", return_value={**value, "probationary": []}):
            with self.assertRaisesRegex(memory.BridgeError, "unexpected recall context"):
                memory.recall(client, "workshop timeout")

    def test_recall_v7_retains_touchstone_facets_and_partial_coverage_atomically(self):
        value = current_context()
        facet = {"schema": "mneme.touchstone-view.v1", "subject": "An authored memory",
                 "coverage": "summary_only", "references": [], "references_omitted": 1,
                 "origins": [{"kind": "direct"}]}
        value.update(primary=[{"id": ID, "summary": "Retained meaning", "touchstone": facet}],
                     partial=True, omitted={"primary": 1})
        value["touchstone_retrieval"].update(searched=True, read_limit=4, catalog_reads=1,
                record_reads=1, referrer_page_reads=1, target_reads=1, anchors_total=3,
                anchors_examined=1, owners_discovered=1, further_tail_unknown=True,
                stop_reason="budget")
        client = FakeClient()
        with patch.object(client, "call_tool", return_value=value):
            result = memory.recall(client, "remembered meaning")
        self.assertIs(result, value)
        self.assertIs(result["primary"][0]["touchstone"], facet)
        self.assertTrue(result["partial"])
        self.assertTrue(result["touchstone_retrieval"]["further_tail_unknown"])

    def test_recall_v7_preserves_envelope_and_coverage_validation(self):
        valid = current_context()
        malformed = [None, {**valid, "touchstone_retrieval": None},
                     {**valid, "touchstone_retrieval": {**valid["touchstone_retrieval"], "target_reads": 1}},
                     {**valid, "touchstone_retrieval": {**valid["touchstone_retrieval"], "anchors_examined": 1}},
                     {**valid, "probationary": []},
                     {**valid, "omitted": {"probationary": 0}},
                     {**valid, "usage": {"probationary": 0}},
                     {**valid, "retrieval": {"lanes": {"probationary": {}}}}]
        for lane in ("core", "primary", "expansions", "episodes"):
            malformed.extend([{**valid, lane: None},
                {**valid, "schema": "mneme.context.v6", lane: [{"id": ID, "touchstone": {}}]}])
        client = FakeClient()
        for value in malformed:
            with self.subTest(value=value), patch.object(client, "call_tool", return_value=value):
                with self.assertRaisesRegex(memory.BridgeError, "unexpected recall context"):
                    memory.recall(client, "task")

    def test_recall_text_remains_bounded_before_native_call(self):
        client = FakeClient()
        for text in (None, "", " ", "é" * 4097):
            with self.subTest(text=text), self.assertRaises(memory.BridgeError):
                memory.recall(client, text)
        self.assertEqual(client.calls, [])

    def test_recall_v6_keeps_historical_reference_view_and_nullable_recorder_atomic(self):
        client = FakeClient()
        old = {"kind": "episode", "id": ID, "edition_id": ID, "episode_id": ID,
               "revision": 0, "current_edition_id": OTHER_ID,
               "occurred": {"kind": "unknown"}, "recorded_at": 10,
               "edition_recorded_at": 10, "thread": None, "recording_session": None,
               "occurrence_contexts": [{"namespace": "place", "key": "old-room", "label": "Pi/Mac"}],
               "origins": [{"kind": "reference", "anchor": {"kind": "semantic", "node_id": OTHER_ID},
                            "from": ID, "to": OTHER_ID, "edge_kind": "Transition",
                            "body_anchor": {"start": 0, "end": 12}}]}
        value = {"schema": "mneme.context.v6", "core": [], "primary": [], "expansions": [],
                 "episodes": [old], "episodic_retrieval": {"state": "not_searched_tag_filter", "mode": "lexical"},
                 "episode_reference_retrieval": {"state": "searched", "further_tail_unknown": True,
                                                "stop_reason": "budget"}}
        # Native v6 admission owns the typed origin/endpoint proof. This bridge
        # passes the complete immutable packet through, not a second interpretation.
        with patch.object(client, "call_tool", return_value=value):
            result = memory.recall(client, "tagged indirect scene")
        self.assertIs(result, value)
        self.assertIs(result["episodes"][0], old)
        self.assertIn("recording_session", result["episodes"][0])
        self.assertIsNone(result["episodes"][0]["recording_session"])
        self.assertEqual(result["episodes"][0]["current_edition_id"], OTHER_ID)
        self.assertEqual(result["episodes"][0]["origins"], old["origins"])
        self.assertEqual(result["episode_reference_retrieval"], value["episode_reference_retrieval"])

    def test_core_bounds_still_checked_at_bridge_boundary(self):
        node = {"id": ID, "summary": "A core lesson", "summary_truncated": False,
                "tags": ["core"], "status": "active", "body": "available",
                "body_truncated": False}
        valid = {"nodes": [node], "total": 1, "truncated": False,
                 "nodes_truncated": False, "bodies_truncated": False,
                 "body_bytes_limit": memory.MAX_CORE_BODY_BYTES}
        self.assertEqual(memory._core_response(valid), valid)
        for bad in ({**valid, "total": 2},
                    {**valid, "nodes": [{**node, "tags": ["other"]}]},
                    {**valid, "nodes": [{**node, "body": "x" * 9000}]}):
            with self.subTest(bad=bad), self.assertRaises(memory.BridgeError):
                memory._core_response(bad)

    def test_read_limits_refuse_before_native_call(self):
        client = FakeClient()
        for limit in (0, True, 1.5, memory.MAX_READ_BODY + 1):
            with self.subTest(limit=limit), self.assertRaises(memory.BridgeError):
                memory.get(client, ID, max_body_bytes=limit)
        self.assertEqual(client.calls, [])

    def test_capture_delegates_entire_verification_to_native(self):
        client = FakeClient()
        result = memory.capture(client, self.value, db="project")
        self.assertEqual(result["readback_status"], "verified")
        self.assertEqual(client.calls, [("capture/verified", "project", self.value)])

    def test_save_delegates_both_kinds_and_retains_manual_identity(self):
        for kind in ("note", "episode"):
            client = FakeClient()
            original = {"kind": kind, "summary": "A new memory"}
            prepared = memory._prepare_save(client, original)
            result = memory.save(client, prepared["payload"], db="user")
            self.assertEqual(result["operation"], "save")
            self.assertEqual(result["kind"], kind)
            self.assertEqual(client.calls, [("save/prepare", original),
                                          ("save/verified", "user", prepared["payload"])])
            self.assertEqual(prepared["payload"]["source"]["key"], "frozen-operation")
            self.assertNotIn("source", original)

    def test_save_does_not_retry_or_lose_accepted_manual_receipt(self):
        client = FakeClient()
        error = McpProtocolError("readback unavailable")
        error.details = {"accepted": {"kind": "episode", "origin": "manual_submission",
                         "operation_id": "stable", "id": ID, "episode_id": ID,
                         "edition_id": ID, "revision": 0, "replayed": False,
                         "readback_status": "failed"}}
        client.failure = error
        with self.assertRaises(memory.BridgeError) as caught:
            memory.save(client, self.value)
        report = caught.exception.as_json()
        self.assertEqual(report["kind"], "episode")
        self.assertEqual(report["origin"], "manual_submission")
        self.assertEqual(report["operation_id"], "stable")
        self.assertEqual(report["edition_id"], ID)
        self.assertFalse(report["retryable"])
        self.assertEqual(len(client.calls), 1)

    def test_save_refuses_incomplete_preparation_or_unverified_receipt(self):
        client = FakeClient()
        for prepared in ({"schema": True, "id": ID, "payload": self.value},
                         {"schema": 1, "id": ID, "payload": {"summary": "no frozen source"}},
                         {"schema": 1, "id": ID, "payload": {**self.value, "db": "user"}}):
            with patch.object(client, "prepare_save", return_value=prepared), \
                    self.assertRaisesRegex(memory.BridgeError, "preparation malformed"):
                memory._prepare_save(client, self.value)
        with patch.object(client, "save_verified", return_value={"id": ID}), \
                self.assertRaisesRegex(memory.BridgeError, "no verified readback"):
            memory.save(client, self.value)

    def test_save_unacknowledged_write_retains_attempt_without_claiming_acceptance(self):
        client = FakeClient()
        attempted = {"kind": "note", "id": ID, "origin": "manual_submission",
                     "operation_id": "frozen", "source": self.value["source"],
                     "db": "project", "write_status": "unacknowledged",
                     "readback_status": "not_attempted", "retryable": False}
        client.failure = McpProtocolError("write response lost")
        client.failure.details = {"attempted": attempted}
        with self.assertRaises(memory.BridgeError) as caught:
            memory.save(client, self.value)
        report = caught.exception.as_json()
        self.assertEqual(report["attempted"], attempted)
        self.assertNotIn("id", report)
        self.assertNotIn("replayed", report)
        self.assertFalse(report["ok"])
        self.assertFalse(report["retryable"])
        self.assertEqual(len(client.calls), 1)

    def test_capture_error_preserves_native_accepted_receipt(self):
        client = FakeClient()
        error = McpProtocolError("readback mismatch")
        error.details = {"kind": "protocol", "message": "readback mismatch",
                         "accepted": {"id": ID, "source": self.value["source"],
                                      "replayed": True, "readback_status": "mismatch",
                                      "retryable": False}}
        client.failure = error
        with self.assertRaises(memory.BridgeError) as caught:
            memory.capture(client, self.value)
        report = caught.exception.as_json()
        self.assertEqual(report["id"], ID)
        self.assertEqual(report["source"], self.value["source"])
        self.assertEqual(report["readback_status"], "mismatch")
        self.assertFalse(report["retryable"])

    def test_capture_input_is_only_byte_bound_and_json_decoded(self):
        path = self.root / "capture.json"
        path.write_text(json.dumps({**self.value, "db": "user"}))
        self.assertIn("db", memory._capture_input(str(path)))
        path.write_bytes(b" " * (memory.MAX_CAPTURE_INPUT + 1))
        with self.assertRaisesRegex(memory.BridgeError, "exceeds 96 KiB"):
            memory._capture_input(str(path))
        path.write_text("[]")
        self.assertEqual(memory._capture_input(str(path)), [])

    def test_episode_reads_use_native_preparation_and_fixed_database(self):
        client = FakeClient()
        value = {"action": "list", "limit": 3}
        prepared = memory._prepare_episode(client, value)
        result = memory.episode(client, prepared, db="user")
        self.assertEqual(result["items"], [])
        self.assertEqual(client.calls, [("episode/prepare", value),
                                        ("episode", {**value, "db": "user"})])

    def test_episode_writes_delegate_all_readback_to_native(self):
        client = FakeClient()
        value = {"action": "revise", **self.value}
        result = memory.episode(client, memory._prepare_episode(client, value))
        self.assertEqual(result["readback_status"], "verified")
        self.assertEqual(result["edition_id"], OTHER_ID)
        self.assertEqual(client.calls, [("episode/prepare", value),
                                        ("episode/verified", "project", value)])

    def test_episode_native_failure_is_not_retried_or_reported_saved(self):
        client = FakeClient()
        value = {"action": "append", **self.value}
        prepared = memory._prepare_episode(client, value)
        error = McpProtocolError("episode exact edition readback failed")
        error.details = {"accepted": {"episode_id": ID, "edition_id": OTHER_ID,
                                      "revision": 0, "replayed": False,
                                      "source": self.value["source"],
                                      "readback_status": "mismatch", "retryable": False}}
        client.failure = error
        with self.assertRaises(memory.BridgeError) as caught:
            memory.episode(client, prepared)
        report = caught.exception.as_json()
        self.assertEqual(report["episode_id"], ID)
        self.assertEqual(report["edition_id"], OTHER_ID)
        self.assertEqual(report["revision"], 0)
        self.assertFalse(report["ok"])
        self.assertFalse(report["retryable"])
        self.assertEqual(report["readback_status"], "mismatch")
        self.assertEqual(sum(call[0] == "episode/verified" for call in client.calls), 1)

    def test_episode_does_not_accept_unverified_success_or_over_budget_read(self):
        client = FakeClient()
        prepared = memory._prepare_episode(client, {"action": "append"})
        with patch.object(client, "episode_verified", return_value={"edition_id": ID}), \
                self.assertRaisesRegex(memory.BridgeError, "no verified readback"):
            memory.episode(client, prepared)
        prepared = memory._prepare_episode(client, {"action": "get"})
        with patch.object(client, "call_tool", return_value={"body": "x" * memory.MAX_EPISODE_RESPONSE_BYTES}), \
                self.assertRaisesRegex(memory.BridgeError, "over budget"):
            memory.episode(client, prepared)

    def test_episode_preparation_never_accepts_native_database_override(self):
        client = FakeClient()
        with self.assertRaisesRegex(memory.BridgeError, "preparation malformed"):
            memory._prepare_episode(client, {"action": "list", "db": "user"})
        self.assertEqual([call[0] for call in client.calls], ["episode/prepare"])

    def test_episode_input_is_only_byte_bound_and_json_decoded(self):
        path = self.root / "episode.json"
        path.write_text('["invalid episode schema; native checks it"]')
        self.assertIsInstance(memory._operation_input(str(path), "episode"), list)
        path.write_bytes(b" " * (memory.MAX_CAPTURE_INPUT + 1))
        with self.assertRaisesRegex(memory.BridgeError, "episode input exceeds 96 KiB"):
            memory._operation_input(str(path), "episode")

    def test_episode_cli_prepares_before_start_and_keeps_configured_store(self):
        for action in ("list", "append"):
            with self.subTest(action=action):
                value = {"action": action, **(self.value if action == "append" else {})}
                path = self.root / "episode.json"
                path.write_text(json.dumps(value))
                prep, session = FakeClient(), FakeClient()
                events = []
                def factory(*args, **kwargs):
                    client = prep if not events else session
                    events.append("construct")
                    class Context:
                        def __enter__(self):
                            return client
                        def __exit__(self, *_):
                            pass
                        def prepare_episode(self, payload):
                            return client.prepare_episode(payload)
                        def close(self):
                            pass
                    return Context()
                output = io.StringIO()
                with patch("memory.McpClient", side_effect=factory), \
                        patch("memory.ensure_ready", side_effect=lambda _: events.append("ensure")), \
                        contextlib.redirect_stdout(output):
                    self.assertEqual(memory.main(["--service-config", str(self.config),
                                                  "episode", "--input", str(path)]), 0)
                self.assertEqual(events, ["construct", "ensure", "construct"])
                self.assertEqual(prep.calls, [("episode/prepare", value)])
                self.assertEqual(session.calls, [("episode/verified", "project", value)]
                                 if action == "append" else [("episode", {**value, "db": "project"})])
                self.assertEqual(json.loads(output.getvalue())["db"], "project")

    def test_episode_invalid_json_null_is_native_validated_before_start(self):
        path = self.root / "episode.json"
        path.write_text('null')
        with patch("memory.McpClient") as factory, patch("memory.ensure_ready") as ensure, \
                contextlib.redirect_stderr(io.StringIO()):
            factory.return_value.prepare_episode.side_effect = McpProtocolError("expected object")
            self.assertEqual(memory.main(["--service-config", str(self.config),
                                          "episode", "--input", str(path)]), 1)
            factory.return_value.prepare_episode.assert_called_once_with(None)
            ensure.assert_not_called()

    def test_episode_no_start_remote_catalog_refusal_prevents_mutation(self):
        config = {"mode": "connect", "url": "http://127.0.0.1:18767/",
                  "database_name": "user", "database_path": "/owner/memory.db"}
        self.config.write_text(json.dumps(config))
        path = self.root / "episode.json"
        value = {"action": "append", **self.value}
        path.write_text(json.dumps(value))
        client = FakeClient(catalog=[{"db": "user", "name": "user", "state": "open",
                                     "configured_path": "/other/memory.db"}])
        with patch("memory.McpClient") as factory, patch("memory.ensure_ready") as ensure, \
                contextlib.redirect_stderr(io.StringIO()) as error:
            factory.return_value.prepare_episode.return_value = {
                "action": "append", "is_mutation": True, "payload": value}
            factory.return_value.__enter__.return_value = client
            self.assertEqual(memory.main(["--service-config", str(self.config), "--no-start",
                                          "episode", "--input", str(path)]), 1)
        ensure.assert_not_called()
        self.assertEqual(client.calls, [("databases", {})])
        report = json.loads(error.getvalue())
        self.assertEqual(report["db"], "user")
        self.assertEqual(report["readback_status"], "not_attempted")
        self.assertIn("catalog verification failed", report["error"])

    def test_cli_prepares_before_ensure_and_write(self):
        path = self.root / "capture.json"
        path.write_text(json.dumps(self.value))
        prep, session = FakeClient(), FakeClient()
        calls = []
        def factory(*args, **kwargs):
            client = prep if not calls else session
            calls.append("construct")
            class Context:
                def __enter__(self):
                    return client
                def __exit__(self, *_):
                    pass
                def prepare_capture(self, value):
                    return client.prepare_capture(value)
                def close(self):
                    pass
            return Context()
        output = io.StringIO()
        with patch("memory.McpClient", side_effect=factory), \
                patch("memory.ensure_ready", side_effect=lambda _: calls.append("ensure")), \
                contextlib.redirect_stdout(output):
            self.assertEqual(memory.main(["--service-config", str(self.config),
                                          "capture", "--input", str(path)]), 0)
        self.assertEqual(calls, ["construct", "ensure", "construct"])
        self.assertEqual(prep.calls, [("capture/prepare", self.value)])
        self.assertEqual(session.calls, [("capture/verified", "project", self.value)])
        self.assertEqual(json.loads(output.getvalue())["db"], "project")

    def test_native_preparation_failure_prevents_ensure(self):
        path = self.root / "capture.json"
        path.write_text(json.dumps(self.value))
        client = FakeClient()
        client.failure = McpProtocolError("invalid capture")
        stderr = io.StringIO()
        with patch("memory.McpClient") as factory, patch("memory.ensure_ready") as ensure, \
                contextlib.redirect_stderr(stderr):
            factory.return_value.prepare_capture.side_effect = client.failure
            self.assertEqual(memory.main(["--service-config", str(self.config),
                                          "capture", "--input", str(path)]), 1)
        ensure.assert_not_called()
        self.assertEqual(json.loads(stderr.getvalue())["readback_status"], "not_attempted")

    def test_save_cli_freezes_payload_before_service_start(self):
        path = self.root / "save.json"
        raw = {"kind": "episode", "summary": "A scene"}
        path.write_text(json.dumps(raw))
        prep, session = FakeClient(), FakeClient()
        output = io.StringIO()
        with patch("memory.McpClient") as factory, patch("memory.ensure_ready") as ensure, \
                contextlib.redirect_stdout(output):
            factory.return_value.prepare_save.side_effect = prep.prepare_save
            factory.return_value.__enter__.return_value = session
            ensure.side_effect = lambda _: self.assertEqual(prep.calls, [("save/prepare", raw)])
            self.assertEqual(memory.main(["--service-config", str(self.config),
                                          "save", "--input", str(path)]), 0)
        self.assertEqual(len(session.calls), 1)
        operation, db, payload = session.calls[0]
        self.assertEqual((operation, db), ("save/verified", "project"))
        self.assertEqual(payload["source"]["key"], "frozen-operation")
        self.assertEqual(payload["kind"], "episode")
        self.assertEqual(json.loads(output.getvalue())["db"], "project")
        self.assertEqual(json.loads(path.read_text()), raw)

    def test_save_invalid_input_refuses_before_service_start(self):
        path = self.root / "save.json"
        path.write_text("null")
        with patch("memory.McpClient") as factory, patch("memory.ensure_ready") as ensure, \
                contextlib.redirect_stderr(io.StringIO()):
            factory.return_value.prepare_save.side_effect = McpProtocolError("expected object")
            self.assertEqual(memory.main(["--service-config", str(self.config),
                                          "save", "--input", str(path)]), 1)
            factory.return_value.prepare_save.assert_called_once_with(None)
            ensure.assert_not_called()


if __name__ == "__main__":
    unittest.main()

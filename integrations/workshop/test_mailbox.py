"""Finite stdio MCP mailbox tests; no model, host daemon, or remote mutation."""

import io
import fcntl
import datetime
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import mailbox
import wake


def rpc(method, ident=None, params=None):
    result = {"jsonrpc": "2.0", "method": method, "params": {} if params is None else params}
    if ident is not None:
        result["id"] = ident
    return (json.dumps(result) + "\n").encode()


def call(name, arguments, ident=2):
    return rpc("tools/call", ident, {"name": name, "arguments": arguments})


INIT = {"protocolVersion": "2025-11-25", "capabilities": {},
        "clientInfo": {"name": "mailbox-test", "version": "1"}}


class MailboxTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()

    def stream(self, *frames):
        sink = io.BytesIO()
        mailbox.run_stream(self.root, io.BytesIO(b"".join(frames)), sink)
        return [json.loads(line) for line in sink.getvalue().splitlines()]

    def tool(self, name, arguments):
        return self.stream(rpc("initialize", 1, INIT), call(name, arguments))[1]["result"]

    def test_finite_transport_catalog_send_retry_read_status_and_eof(self):
        first = self.stream(rpc("initialize", 1, INIT),
                            rpc("notifications/initialized"), rpc("tools/list", 2),
                            call("send_message", {"request_id": "other-codex-001", "text": "What did you learn?"}, 3),
                            call("read_reply", {"request_id": "other-codex-001"}, 4),
                            call("workshop_status", {}, 5))
        self.assertEqual(len(first), 5)
        self.assertEqual(first[0]["result"]["protocolVersion"], "2025-11-25")
        self.assertEqual({item["name"] for item in first[1]["result"]["tools"]},
                         {"send_message", "read_reply", "workshop_status"})
        self.assertTrue(first[2]["result"]["structuredContent"]["accepted"])
        self.assertEqual(first[3]["result"]["structuredContent"]["status"], "pending")
        self.assertEqual(first[4]["result"]["structuredContent"]["queued_mailbox_messages"], 1)
        record = wake.active_records(self.root)[0]
        self.assertEqual(record["event"]["source"], mailbox.SOURCE)
        self.assertEqual(record["event"]["body"], "What did you learn?")
        again = self.tool("send_message", {"request_id": "other-codex-001", "text": "What did you learn?"})
        self.assertEqual(again["structuredContent"]["event_status"], "pending")
        self.assertEqual(len(wake.active_records(self.root)), 1)
        conflict = self.tool("send_message", {"request_id": "other-codex-001", "text": "Different"})
        self.assertTrue(conflict["isError"])

    def test_completed_receipt_only_and_identity_guard(self):
        self.tool("send_message", {"request_id": "q1", "text": "A question"})
        run_id = "run-001"
        wake.claim(self.root, run_id)
        stage = self.root / "runs" / run_id / "orientation"
        stage.mkdir(parents=True)
        reply = {"source": mailbox.SOURCE, "event_id": "q1", "text": "An answer."}
        (stage / "result.json").write_text(json.dumps({"replies": [reply]}))
        self.assertIsNone(self.tool("read_reply", {"request_id": "q1"})["structuredContent"]["reply"])
        run = stage.parent
        lock_path = self.root / ".workshop.lock"
        lock_path.touch()
        (run / "result.json").write_text(json.dumps({"replies": [reply]}))
        receipt = {"schema": "mneme.workshop.receipt.v2", "run_id": run_id,
                   "status": "running", "event_ids": [{"source": mailbox.SOURCE, "event_id": "q1"}]}
        (run / "receipt.json").write_text(json.dumps(receipt))
        self.assertIsNone(self.tool("read_reply", {"request_id": "q1"})["structuredContent"]["reply"])
        receipt["status"] = "completed"
        (run / "receipt.json").write_text(json.dumps(receipt))
        with lock_path.open("rb") as active:
            fcntl.flock(active, fcntl.LOCK_EX)
            self.assertIsNone(self.tool("read_reply", {"request_id": "q1"})["structuredContent"]["reply"])
        got = self.tool("read_reply", {"request_id": "q1"})["structuredContent"]
        self.assertEqual(got, {"request_id": "q1", "status": "replied", "reply": "An answer."})
        receipt["event_ids"] = []
        (run / "receipt.json").write_text(json.dumps(receipt))
        self.assertTrue(self.tool("read_reply", {"request_id": "q1"})["isError"])

    def test_completed_without_reply_not_fabricated(self):
        self.tool("send_message", {"request_id": "q2", "text": "Question"})
        run_id = "run-002"
        wake.claim(self.root, run_id)
        run = self.root / "runs" / run_id
        run.mkdir(parents=True)
        (self.root / ".workshop.lock").touch()
        (run / "result.json").write_text(json.dumps({"replies": []}))
        (run / "receipt.json").write_text(json.dumps({"schema": "mneme.workshop.receipt.v2",
            "run_id": run_id, "status": "completed",
            "event_ids": [{"source": mailbox.SOURCE, "event_id": "q2"}]}))
        got = self.tool("read_reply", {"request_id": "q2"})["structuredContent"]
        self.assertEqual(got["status"], "completed_without_reply")
        self.assertIsNone(got["reply"])

    def test_archived_lookup_preserves_request_specific_terminal_outcome(self):
        outcomes = {"q1": {"status": "replied", "reply": "First answer"},
                    "q2": {"status": "replied", "reply": "Second answer"},
                    "q3": {"status": "completed_without_reply", "reply": None}}
        def archived(_root, source, event_id):
            self.assertEqual(source, mailbox.SOURCE)
            return {"location": "archive",
                    "record": {"event": {"source": source, "event_id": event_id}},
                    "terminal": outcomes[event_id]}
        with patch.object(wake, "lookup", side_effect=archived):
            # No runner lock or old run file exists: immutable archive is enough.
            for request_id in ("q2", "q1", "q3"):
                got = self.tool("read_reply", {"request_id": request_id})["structuredContent"]
                self.assertEqual(got, {"request_id": request_id, **outcomes[request_id]})

    def test_operator_archive_keeps_replies_and_idempotency_after_run_moves(self):
        for request_id in ("q1", "q2", "q3"):
            self.tool("send_message", {"request_id": request_id, "text": "Question " + request_id})
        run_id = "run-old"
        claimed = wake.claim(self.root, run_id)
        wake.delivered(self.root, run_id)
        run = self.root / "runs" / run_id
        run.mkdir(parents=True)
        identities = [{"source": mailbox.SOURCE, "event_id": item["event"]["event_id"]}
                      for item in claimed["events"]]
        yesterday = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=1)
        receipt = {"schema": "mneme.workshop.receipt.v2", "run_id": run_id,
                   "status": "completed", "event_count": 3, "event_ids": identities,
                   "started_at": yesterday.isoformat()}
        result = {"status": "progress", "summary": "Answered two questions.",
                  "artifacts": [], "next_step": "", "memory_candidates": [],
                  "next_wake_seconds": 3600,
                  "replies": [{"source": mailbox.SOURCE, "event_id": "q2", "text": "Second answer"},
                              {"source": mailbox.SOURCE, "event_id": "q1", "text": "First answer"}]}
        (run / "receipt.json").write_text(json.dumps(receipt))
        (run / "result.json").write_text(json.dumps(result))
        (self.root / "PAUSED").touch()
        archived = wake.archive(self.root)
        self.assertEqual(archived["archived_events"], 3)
        self.assertFalse(run.exists())
        self.assertEqual(wake.active_records(self.root), [])
        for request_id, expected in (("q1", "First answer"), ("q2", "Second answer")):
            got = self.tool("read_reply", {"request_id": request_id})["structuredContent"]
            self.assertEqual(got, {"request_id": request_id, "status": "replied", "reply": expected})
        self.assertEqual(self.tool("read_reply", {"request_id": "q3"})["structuredContent"],
                         {"request_id": "q3", "status": "completed_without_reply", "reply": None})
        retry = self.tool("send_message", {"request_id": "q1", "text": "Question q1"})
        self.assertTrue(retry["structuredContent"]["accepted"])
        self.assertEqual(wake.active_records(self.root), [])
        conflict = self.tool("send_message", {"request_id": "q1", "text": "Changed"})
        self.assertTrue(conflict["isError"])
        self.tool("send_message", {"request_id": "q4", "text": "New question"})
        self.assertEqual(wake.active_records(self.root)[0]["sequence"], 4)

    def test_current_v3_completed_reply_and_archive(self):
        self.tool("send_message", {"request_id": "qv3", "text": "Current-cycle question"})
        batch = wake.claim(self.root, "run-v3", cycle_class="background")
        wake.delivered(self.root, "run-v3")
        run = self.root / "runs" / "run-v3"
        run.mkdir(parents=True)
        identity = {"source": mailbox.SOURCE, "event_id": "qv3"}
        (self.root / ".workshop.lock").touch()
        (run / "receipt.json").write_text(json.dumps({"schema": "mneme.workshop.receipt.v3",
            "cycle_class": "background", "run_id": "run-v3", "status": "completed",
            "started_at": (datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=2)).isoformat(),
            "event_count": 1, "event_ids": [identity]}))
        (run / "result.json").write_text(json.dumps({"status": "progress", "summary": "answered",
            "artifacts": [], "next_step": "", "memory_candidates": [], "next_wake_seconds": 3600,
            "replies": [{**identity, "text": "Current reply"}]}))
        self.assertEqual(self.tool("read_reply", {"request_id": "qv3"})["structuredContent"]["reply"],
                         "Current reply")
        (self.root / "PAUSED").touch()
        self.assertEqual(wake.archive(self.root)["archived_events"], 1)
        self.assertEqual(self.tool("read_reply", {"request_id": "qv3"})["structuredContent"]["reply"],
                         "Current reply")

    def test_status_separates_background_and_interactive_quotas(self):
        now = datetime.datetime.now(datetime.timezone.utc)
        runs = self.root / "runs"
        runs.mkdir()
        for name, schema, cycle_class in (("old-v2", "mneme.workshop.receipt.v2", None),
                                          ("new-bg", "mneme.workshop.receipt.v3", "background"),
                                          ("new-chat", "mneme.workshop.receipt.v3", "interactive")):
            run = runs / name
            run.mkdir()
            receipt = {"schema": schema, "run_id": name, "status": "completed",
                       "started_at": now.isoformat()}
            if cycle_class is not None:
                receipt["cycle_class"] = cycle_class
            (run / "receipt.json").write_text(json.dumps(receipt))
        status = self.tool("workshop_status", {})["structuredContent"]
        self.assertEqual(status["starts_today"], 2)
        self.assertEqual(status["interactive_starts_today"], 1)
        self.assertEqual(status["interactive_starts_last_hour"], 1)

    def test_status_absent_configuration_is_unknown_and_read_only(self):
        status = self.tool("workshop_status", {})["structuredContent"]
        self.assertFalse(status["runner_configuration_known"])
        self.assertIsNone(status["max_starts_utc_day"])
        self.assertIsNone(status["next_due_at"])
        self.assertIsNone(status["last_observed_cycle"])
        self.assertEqual(status["stored_relative_next_due_at"], "1970-01-01T00:00:00+00:00")
        self.assertEqual(list(self.root.iterdir()), [])

    def test_status_mailbox_count_uses_exact_shared_inbox_contract(self):
        self.tool("send_message", {"request_id": "pending", "text": "one pending message"})
        for number, source, kind in ((1, wake.MAILBOX_SOURCE, "wrong-kind"),
                                     (2, wake.SIGNAL_SOURCE, wake.SIGNAL_KIND),
                                     (3, "technical", wake.MAILBOX_KIND)):
            wake.enqueue(self.root, {"source": source, "kind": kind, "event_id": str(number),
                                     "summary": "", "body": "", "reference": ""})
        before = (self.root / "events.json").read_bytes()
        status = self.tool("workshop_status", {})["structuredContent"]
        self.assertEqual(status["queued_mailbox_messages"], 1)
        self.assertEqual(status["queued_mailbox_messages"],
                         wake.inbox_summary(self.root)["pending_mailbox_messages"])
        self.assertEqual((self.root / "events.json").read_bytes(), before)

    def test_status_reports_hourly_unlimited_receipt_as_observed_not_configured(self):
        now = datetime.datetime.now(datetime.timezone.utc)
        runs = self.root / "runs"
        old = runs / "z-old-relative"
        old.mkdir(parents=True)
        (old / "receipt.json").write_text(json.dumps({
            "schema": "mneme.workshop.receipt.v2", "run_id": old.name,
            "status": "completed", "started_at": (now - datetime.timedelta(hours=2)).isoformat(),
            "max_starts_utc_day": 4}))
        current = runs / "a-hourly"
        current.mkdir()
        receipt = {"schema": "mneme.workshop.receipt.v3", "run_id": current.name,
                   "status": "running", "started_at": now.isoformat(), "cycle_class": "background",
                   "background_schedule_mode": "hourly", "max_starts_utc_day": None,
                   "max_starts_rolling_hour": None}
        path = current / "receipt.json"
        path.write_text(json.dumps(receipt))
        before = path.read_bytes()
        status = self.tool("workshop_status", {})["structuredContent"]
        self.assertFalse(status["runner_configuration_known"])
        self.assertIsNone(status["max_starts_utc_day"])
        self.assertIsNone(status["next_due_at"])
        observed = status["last_observed_cycle"]
        self.assertEqual(observed["source"], "latest_retained_run_receipt")
        self.assertEqual(observed["run_id"], current.name)
        self.assertEqual(observed["policy"], {"background_schedule_mode": "hourly",
                                            "max_starts_utc_day": None,
                                            "max_starts_rolling_hour": None})
        self.assertEqual(path.read_bytes(), before)
        self.assertFalse((self.root / "state.json").exists())
        self.assertFalse((self.root / "hourly-state.json").exists())

        receipt.update(cycle_class="interactive", interactive_budget_mode="unlimited")
        path.write_text(json.dumps(receipt))
        observed = self.tool("workshop_status", {})["structuredContent"]["last_observed_cycle"]
        self.assertEqual(observed["cycle_class"], "interactive")
        self.assertEqual(observed["policy"]["interactive_budget_mode"], "unlimited")

    def test_status_does_not_invent_policy_for_legacy_receipt(self):
        run = self.root / "runs" / "old-v2"
        run.mkdir(parents=True)
        (run / "receipt.json").write_text(json.dumps({
            "schema": "mneme.workshop.receipt.v2", "run_id": run.name,
            "status": "completed", "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat()}))
        observed = self.tool("workshop_status", {})["structuredContent"]["last_observed_cycle"]
        self.assertEqual(observed["cycle_class"], "background")
        self.assertEqual(observed["policy"], {})

    def test_parser_dispatch_refusals_and_bounds(self):
        outputs = self.stream(rpc("tools/list", 1), rpc("initialize", 2, INIT),
                              rpc("tools/call", 3, {"name": "pause", "arguments": {}}),
                              call("send_message", {"request_id": "q", "text": ""}, 4),
                              call("send_message", {"request_id": "q", "text": "x", "extra": 1}, 5),
                              rpc("tools/list", 6, {"cursor": "x"}),
                              rpc("resources/list", 7))
        self.assertEqual(outputs[0]["error"]["code"], -32000)
        self.assertEqual(outputs[2]["error"]["code"], -32602)
        self.assertTrue(outputs[3]["result"]["isError"])
        self.assertTrue(outputs[4]["result"]["isError"])
        self.assertEqual(outputs[5]["error"]["code"], -32602)
        self.assertEqual(outputs[6]["error"]["code"], -32601)
        self.assertEqual(list(self.root.iterdir()), [])
        oversized = b" " * (mailbox.MAX_FRAME + 1) + b"\n"
        self.assertEqual(self.stream(oversized, rpc("initialize", 1, INIT))[0]["error"]["code"], -32600)
        self.assertEqual(self.stream(b'{"jsonrpc":"2.0","id":1,"id":2,"method":"initialize"}\n')[0]["error"]["code"], -32700)

    def test_codex_0157_list_metadata_and_call_envelope(self):
        # Exact harmless Codex 0.157.0 tools/list frame observed at startup.
        observed = b'{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"progressToken":0}}}\n'
        output = self.stream(rpc("initialize", 0, INIT), observed,
                             rpc("tools/call", 2, {"name": "workshop_status", "arguments": {},
                                                    "_meta": {"progressToken": "status-2"}}))
        catalog = output[1]["result"]
        self.assertEqual({tool["name"] for tool in catalog["tools"]},
                         {"send_message", "read_reply", "workshop_status"})
        self.assertNotIn("nextCursor", catalog)
        self.assertIn("paused", output[2]["result"]["structuredContent"])
        self.assertEqual(list(self.root.iterdir()), [])

        # Codex 0.157.0 genuine tools/call keys/types, captured by a shim that
        # did not forward the call. Values here are neutral fixture data.
        codex_meta = {"callId": "c" * 41, "threadId": "t" * 36,
                      "sessionId": "s" * 36, "windowId": "w" * 38,
                      "itemId": "i" * 54, "progressToken": 7,
                      "x-codex-turn-metadata": {
                          "session_id": "s" * 36, "thread_id": "t" * 36,
                          "turn_id": "u" * 36, "reasoning_effort": "low",
                          "model": "test-model", "thread_source": "exec",
                          "turn_trigger": "user", "sandbox": "workspace",
                          "sandbox_mode": "workspace",
                          "auto_review_enabled": False,
                          "node_repl_auto_review_required": False,
                          "node_repl_disabled": True,
                          "turn_started_at_unix_ms": 1, "codex_version": "0.157.0"}}
        call_response = self.stream(rpc("initialize", 0, INIT),
                                    rpc("tools/call", 1, {"name": "workshop_status",
                                                          "arguments": {}, "_meta": codex_meta}))[1]
        self.assertIn("paused", call_response["result"]["structuredContent"])
        self.assertEqual(list(self.root.iterdir()), [])
        vendor_list = self.stream(rpc("initialize", 0, INIT),
                                  rpc("tools/list", 1, {"_meta": codex_meta}))[1]
        self.assertEqual(len(vendor_list["result"]["tools"]), 3)
        extension = {"com.example/debug": {"flags": [True, None]}}
        extension_responses = self.stream(
            rpc("initialize", 0, INIT), rpc("tools/list", 1, {"_meta": extension}),
            rpc("tools/call", 2, {"name": "workshop_status", "arguments": {}, "_meta": extension}))
        self.assertEqual(len(extension_responses[1]["result"]["tools"]), 3)
        self.assertIn("paused", extension_responses[2]["result"]["structuredContent"])

        bad = [
            {"_meta": {"progressToken": True}},
            {"_meta": {"progressToken": 1.5}},
            {"_meta": {"progressToken": ""}},
            {"_meta": {"progressToken": "x" * 129}},
            {"_meta": {"progressToken": 2 ** 64}},
            {"_meta": []},
            {"cursor": "unissued"},
            {"unknown": "value"},
        ]
        for params in bad:
            with self.subTest(params=params):
                response = self.stream(rpc("initialize", 0, INIT), rpc("tools/list", 1, params))[1]
                self.assertEqual(response["error"]["code"], -32602)
        denied = self.stream(rpc("initialize", 0, INIT),
                             rpc("tools/call", 1, {"name": "send_message",
                                                   "arguments": {"request_id": "q", "text": "No side effect"},
                                                   "_meta": {"progressToken": False}}))[1]
        self.assertEqual(denied["error"]["code"], -32602)
        self.assertEqual(list(self.root.iterdir()), [])
        too_deep = {"value": 0}
        for _ in range(13):
            too_deep = {"nested": too_deep}
        response = self.stream(rpc("initialize", 0, INIT),
                               rpc("tools/list", 1, {"_meta": too_deep}))[1]
        self.assertEqual(response["error"]["code"], -32700)

    def test_supported_versions_and_subprocess_stdio(self):
        for version in mailbox.VERSIONS:
            with self.subTest(version=version):
                init = dict(INIT, protocolVersion=version)
                self.assertEqual(self.stream(rpc("initialize", 1, init))[0]["result"]["protocolVersion"], version)
        command = [sys.executable, str(Path(mailbox.__file__)), "--root", str(self.root)]
        process = subprocess.run(command, input=rpc("initialize", 1, INIT) + rpc("tools/list", 2),
                                 capture_output=True, timeout=5)
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(len(process.stdout.splitlines()), 2)
        self.assertEqual(process.stderr, b"")


if __name__ == "__main__":
    unittest.main()

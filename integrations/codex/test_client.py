import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from mcp_client import McpClient, McpProtocolError, McpToolError, McpTransportError

SCRIPT = """#!/usr/bin/env python3
import json, sys
for raw in sys.stdin:
    request = json.loads(raw)
    op = request["op"]
    if op == "connect":
        result = {"protocolVersion": "2025-11-25", "serverInfo": {"name": "mneme-mcp"},
                  "_mneme_client": {"expected_db_id": 1, "save": 1}}
    elif op == "tools/list":
        result = {"tools": [{"name": "capture"}]}
    elif op == "capture/prepare":
        result = {"id": "test-id", "digest": "test-digest"}
    elif op == "capture/verified":
        result = {"id": "test-id", "replayed": False, "readback_status": "verified"}
    elif op == "save/prepare":
        result = {"schema": 1, "id": "test-id", "payload": request["payload"]}
    elif op == "save/verified":
        result = {"id": "test-id", "kind": "note", "origin": "provided_source",
                  "replayed": False, "readback_status": "verified"}
    elif op == "episode/prepare":
        result = {"action": "append", "is_mutation": True, "payload": request["payload"]}
    elif op == "episode/verified":
        result = {"episode_id": "test-episode", "edition_id": "test-edition",
                  "replayed": False, "readback_status": "verified"}
    elif op == "rpc":
        result = {"content": [{"type": "text", "text": "raw"}], "isError": False}
    elif request.get("name") == "refuse":
        print(json.dumps({"id": request["id"], "ok": False,
                          "error": {"kind": "tool", "message": "capability denied"}}), flush=True)
        continue
    else:
        result = {"schema": request.get("arguments", {}).get("test_schema", "mneme.context.v5"),
                  "episodes": [{"kind": "episode", "edition_id": "edition", "episode_id": "root"}],
                  "episodic_retrieval": {"state": "searched", "mode": "lexical"}}
    native = {"_mneme_client": {"expected_db_id": 1, "save": 1}} if op == "connect" else {}
    print(json.dumps({"id": request["id"], "ok": True, "result": result, **native}), flush=True)
"""


class ClientTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        binary = Path(self.temp.name) / "mnemed"
        binary.write_text(SCRIPT)
        binary.chmod(0o700)
        override = patch.dict(os.environ, {"MNEME_CLIENT_BINARY": str(binary)})
        override.start()
        self.addCleanup(override.stop)
        self.url = "http://127.0.0.1:12345/"

    def test_owner_capabilities_are_exact_local_metadata_only(self):
        for marker,expected in ((None,False),({},False),
                ({"retag_content_guards":1,"tag_vocabulary":"true"},False),
                ({"retag_content_guards":True,"tag_vocabulary":True},True)):
            client=McpClient(self.url)
            client._connected=True
            client._native_capabilities={"owner_capabilities":marker}
            client.connect_result={"_mneme_client":{"owner_capabilities":{
                "retag_content_guards":True,"tag_vocabulary":True}}}
            with patch.object(client,"list_tools") as catalog:
                result=client.owner_capabilities()
            self.assertEqual(result,{"retag_content_guards":expected,"tag_vocabulary":expected})
            catalog.assert_not_called()
            client.close()
            self.assertEqual(client._native_capabilities,{})

    def test_native_owner_summary_survives_envelope_and_downgrade_reconnect(self):
        binary=Path(self.temp.name)/"mnemed"
        old='native = {"_mneme_client": {"expected_db_id": 1, "save": 1}} if op == "connect" else {}'
        new='native = {"_mneme_client": {"expected_db_id": 1, "save": 1, "owner_capabilities": {"retag_content_guards": True, "tag_vocabulary": True}}} if op == "connect" else {}'
        binary.write_text(SCRIPT.replace(old,new))
        client=McpClient(self.url)
        self.assertTrue(all(client.owner_capabilities().values()))
        client.close()
        # A stale bridge forwarding spoofed remote initialize data is not support.
        binary.write_text(SCRIPT.replace('"_mneme_client": {"expected_db_id": 1, "save": 1}}',
            '"_mneme_client": {"expected_db_id": 1, "save": 1, "owner_capabilities": {"retag_content_guards": True, "tag_vocabulary": True}}}',1))
        self.assertFalse(any(client.owner_capabilities().values()))
        client.close()

    def test_native_operations_and_lifetime(self):
        with McpClient(self.url) as client:
            self.assertEqual(client.connect_result["serverInfo"]["name"], "mneme-mcp")
            self.assertEqual(client.list_tools()[0]["name"], "capture")
            context = client.call_tool("recall_context", {})
            self.assertEqual(context["schema"], "mneme.context.v5")
            self.assertEqual(context["episodes"][0]["kind"], "episode")
            self.assertEqual(context["episodic_retrieval"]["mode"], "lexical")
            self.assertEqual(client.raw_rpc("tools/call")["content"][0]["text"], "raw")
            self.assertEqual(client.prepare_capture({"summary": "x"})["digest"], "test-digest")
            self.assertEqual(client.capture_verified("project", {})["readback_status"], "verified")
            self.assertEqual(client.prepare_episode({"action": "append"})["payload"], {"action": "append"})
            self.assertEqual(client.episode_verified("project", {})["readback_status"], "verified")
            self.assertEqual(client.prepare_save({"summary": "x"})["schema"], 1)
            self.assertEqual(client.save_verified("project", {})["readback_status"], "verified")
            for operation in ("save", "capture", "episode"):
                self.assertEqual(getattr(client, operation + "_verified")(
                    "project", {}, expected_db_id="00000000000000000000000029"
                )["readback_status"], "verified")
        self.assertIsNone(client._process)
        self.assertEqual(client._native_capabilities, {})

    def test_episode_preparation_is_pure_and_write_stays_in_one_session(self):
        client = McpClient(self.url)
        with patch.object(client, "_request", return_value={}) as request:
            client.prepare_episode({"action": "list"})
            request.assert_called_once_with("episode/prepare", payload={"action": "list"})
            self.assertFalse(client._connected)
            request.reset_mock()
            client.episode_verified("project", {"action": "append"})
            self.assertEqual([call.args[0] for call in request.call_args_list],
                             ["connect", "episode/verified"])
            request.assert_called_with("episode/verified", db="project", payload={"action": "append"})
            client.episode_verified("project", {"action": "revise"})
            self.assertEqual(sum(call.args[0] == "connect" for call in request.call_args_list), 1)

    def test_verified_guard_is_routing_metadata_not_payload(self):
        guard = "00000000000000000000000029"
        for operation in ("save", "capture", "episode"):
            client = McpClient(self.url)
            client._native_capabilities = {"expected_db_id": 1, "save": 1}
            payload = {"summary": "unchanged"}
            with patch.object(client, "_request", side_effect=[
                {"_mneme_client": {"expected_db_id": 1}}, {"verified": True},
            ]) as request:
                result = getattr(client, operation + "_verified")(
                    "project", payload, expected_db_id=guard)
                self.assertEqual(result, {"verified": True})
                request.assert_called_with(operation + "/verified", db="project",
                                           payload=payload, expected_db_id=guard)
            self.assertEqual(payload, {"summary": "unchanged"})

    def test_old_or_malformed_bridge_marker_refuses_guard_before_write(self):
        for marker in (None, {}, {"expected_db_id": True}, {"expected_db_id": "1"},
                       {"expected_db_id": 2}):
            for operation in ("save", "capture", "episode"):
                client = McpClient(self.url)
                client._native_capabilities = {**(marker if isinstance(marker, dict) else {}), "save": 1}
                with patch.object(client, "_request", return_value={"_mneme_client": marker}) as request:
                    with self.assertRaisesRegex(McpProtocolError, "guarded write was not sent"):
                        getattr(client, operation + "_verified")(
                            "project", {}, expected_db_id="00000000000000000000000029")
                    request.assert_called_once_with("connect")
                    # Existing unguarded callers remain usable, without a silent
                    # fallback for the refused guarded operation.
                    getattr(client, operation + "_verified")("project", {})
                    request.assert_called_with(operation + "/verified", db="project", payload={})

    def test_old_bridge_cannot_forward_remote_initialize_marker_as_native_support(self):
        binary = Path(self.temp.name) / "mnemed"
        binary.write_text(SCRIPT.replace(
            'native = {"_mneme_client": {"expected_db_id": 1, "save": 1}} if op == "connect" else {}',
            'native = {}'))
        for operation in ("capture", "episode"):
            with McpClient(self.url) as client:
                # The old bridge forwarded this server-controlled result, but
                # did not emit a bridge-owned marker on the NDJSON envelope.
                self.assertEqual(client.connect_result["_mneme_client"]["expected_db_id"], 1)
                with patch.object(client, "_request", wraps=client._request) as request:
                    with self.assertRaisesRegex(McpProtocolError, "guarded write was not sent"):
                        getattr(client, operation + "_verified")(
                            "project", {}, expected_db_id="00000000000000000000000029")
                    request.assert_not_called()
                with self.assertRaisesRegex(McpProtocolError, "lacks SAVE support"):
                    client.save_verified("project", {})

    def test_concern_checked_thin_atomic_forwarding_no_retry(self):
        client = McpClient(self.url)
        client._connected = True
        client._native_capabilities = {"concern": 1, "expected_db_id": 1}
        payload = {"action": "notice", "notice": {"native": "checked"}}
        ack = {"db": "project", "db_id": "00000000000000000000000029",
               "action": "notice", "outcome": {"status": "refused", "reason": "stale_meanings", "row": None}}
        with patch.object(client, "_request", return_value=ack) as request:
            self.assertEqual(client.concern_checked("project", payload, expected_db_id=ack["db_id"]), ack)
            request.assert_called_once_with("concern/checked", db="project", payload=payload,
                                            expected_db_id=ack["db_id"])
        with patch.object(client, "_request", side_effect=McpProtocolError("invalid atomic result")) as request:
            with self.assertRaisesRegex(McpProtocolError, "atomic result"):
                client.concern_checked("project", payload, expected_db_id=ack["db_id"])
            self.assertEqual(request.call_count, 1)
        with patch.object(client, "_request", return_value={}) as request:
            client.concern_checked("project", {"action": "list", "endpoint": "native"})
            request.assert_called_once_with("concern/checked", db="project",
                                            payload={"action": "list", "endpoint": "native"})

    def test_concern_native_marker_is_required_and_not_server_spoofable(self):
        for version in (None, False, True, "1", 0, 2, {}, []):
            client = McpClient(self.url)
            client._connected = True
            client._native_capabilities = {"concern": version, "expected_db_id": 1}
            with patch.object(client, "_request") as request:
                with self.assertRaisesRegex(McpProtocolError, "concern support"):
                    client.concern_checked("project", {"action": "notice"}, expected_db_id="native")
                request.assert_not_called()
        script = SCRIPT.replace('"save": 1}', '"save": 1, "concern": 1}').replace(
            'native = {"_mneme_client": {"expected_db_id": 1, "save": 1, "concern": 1}} if op == "connect" else {}',
            'native = {}')
        (Path(self.temp.name) / "mnemed").write_text(script)
        with McpClient(self.url) as client:
            self.assertEqual(client.connect_result["_mneme_client"]["concern"], 1)
            with self.assertRaisesRegex(McpProtocolError, "concern support"):
                client.concern_checked("project", {"action": "list"})

    def test_save_preparation_is_pure_and_no_legacy_fallback(self):
        client = McpClient(self.url)
        with patch.object(client, "_request", return_value={}) as request:
            payload = {"kind": "episode", "summary": "A scene"}
            client.prepare_save(payload)
            request.assert_called_once_with("save/prepare", payload=payload)
            self.assertFalse(client._connected)
        client._connected = True
        client._native_capabilities = {"save": 1}
        with patch.object(client, "_request", side_effect=McpProtocolError("unsupported SAVE")) as request:
            with self.assertRaisesRegex(McpProtocolError, "unsupported SAVE"):
                client.save_verified("project", {"summary": "A note"})
            request.assert_called_once_with("save/verified", db="project", payload={"summary": "A note"})

    def test_missing_or_malformed_native_save_support_refuses_before_write(self):
        for version in (None, True, "1", 2):
            client = McpClient(self.url)
            client._connected = True
            client._native_capabilities = {"expected_db_id": 1, "save": version}
            with patch.object(client, "_request") as request:
                with self.assertRaisesRegex(McpProtocolError, "lacks SAVE support"):
                    client.save_verified("project", {}, expected_db_id="00000000000000000000000029")
                request.assert_not_called()

    def test_refusal_is_typed_and_does_not_kill_session(self):
        with McpClient(self.url) as client:
            with self.assertRaisesRegex(McpToolError, "capability denied"):
                client.call_tool("refuse", {})
            self.assertEqual(client.list_tools()[0]["name"], "capture")

    def test_local_only_and_bounded_request(self):
        for url in ("https://example.com/mcp", "http://localhost:1234/"):
            with self.assertRaises(ValueError):
                McpClient(url)
        with McpClient(self.url) as client:
            with self.assertRaisesRegex(McpProtocolError, "128 KiB"):
                client.call_tool("x", {"text": "x" * (130 * 1024)})

    def test_child_exit_is_transport_error(self):
        binary = Path(self.temp.name) / "exit"
        binary.write_text("#!/bin/sh\nexit 0\n")
        binary.chmod(0o700)
        with patch.dict(os.environ, {"MNEME_CLIENT_BINARY": str(binary)}):
            with self.assertRaises(McpTransportError):
                McpClient(self.url).connect()

    def test_nonreading_child_cannot_block_pipe_write(self):
        binary = Path(self.temp.name) / "sleep"
        binary.write_text("#!/usr/bin/env python3\nimport time\ntime.sleep(60)\n")
        binary.chmod(0o700)
        with patch.dict(os.environ, {"MNEME_CLIENT_BINARY": str(binary)}):
            client = McpClient(self.url, timeout=0.2)
            with self.assertRaisesRegex(McpTransportError, "timed out"):
                client.prepare_capture({"body": "x" * (100 * 1024)})
            self.assertIsNone(client._process)

    def test_token_stays_out_of_argv(self):
        with patch("mcp_client.subprocess.Popen", side_effect=OSError("stop")) as spawn:
            with self.assertRaises(McpTransportError):
                McpClient(self.url, token="private-token").connect()
        self.assertNotIn("private-token", spawn.call_args.args[0])
        self.assertEqual(spawn.call_args.kwargs["env"]["MNEME_CLIENT_EPHEMERAL_TOKEN"], "private-token")


if __name__ == "__main__":
    unittest.main()

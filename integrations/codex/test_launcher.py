import io
import json
from types import SimpleNamespace
import unittest

from launcher import MAX_REQUEST_BYTES, run_stream
from mcp_client import McpTransportError


def frame(method, ident=None, params=None):
    value = {"jsonrpc": "2.0", "method": method, "params": params or {}}
    if ident is not None:
        value["id"] = ident
    if method == "initialize" and params is None:
        value["params"] = {"protocolVersion": "2025-11-25", "capabilities": {}}
    return json.dumps(value).encode() + b"\n"


class FakeClient:
    instances = []
    fail_next = False

    def __init__(self, url, *, token_env=None, timeout=30):
        self.calls = []
        self.connect_result = {"protocolVersion": "2025-11-25",
                               "serverInfo": {"name": "mneme-mcp"}}
        self.instances.append(self)

    def connect(self):
        self.calls.append("connect")
        return self

    def raw_rpc(self, method, params):
        self.calls.append((method, params))
        if self.fail_next:
            type(self).fail_next = False
            raise McpTransportError("ambiguous write")
        return ({"tools": [{"name": "capture"}]} if method == "tools/list"
                else {"content": [], "isError": False})

    def close(self):
        self.calls.append("close")


class LauncherTests(unittest.TestCase):
    def setUp(self):
        FakeClient.instances = []
        FakeClient.fail_next = False
        self.config = SimpleNamespace(url="http://127.0.0.1:12345/", token_env="")
        self.ensures = []

    def run_frames(self, *frames, ensure=None):
        sink = io.BytesIO()
        run_stream(self.config, io.BytesIO(b"".join(frames)), sink,
                   ensure=ensure or self.ensures.append, client_factory=FakeClient)
        return [json.loads(line) for line in sink.getvalue().splitlines()]

    def test_valid_initialize_ensures_and_relays_raw_result(self):
        replies = self.run_frames(frame("initialize", 1), frame("notifications/initialized"),
                                  frame("tools/list", 2))
        self.assertEqual([reply["id"] for reply in replies], [1, 2])
        self.assertEqual(replies[1]["result"]["tools"][0]["name"], "capture")
        self.assertEqual(len(self.ensures), 1)
        self.assertEqual(FakeClient.instances[0].calls,
                         ["connect", ("tools/list", {}), "close"])

    def test_older_supported_outer_version_is_virtualized(self):
        replies = self.run_frames(frame("initialize", 1, {"protocolVersion": "2025-06-18"}),
                                  frame("ping", 2), frame("tools/list", 3))
        self.assertEqual(replies[0]["result"]["protocolVersion"], "2025-06-18")
        self.assertIn("result", replies[1])
        self.assertEqual(replies[2]["result"]["tools"][0]["name"], "capture")

    def test_bad_frames_never_start_host(self):
        replies = self.run_frames(b'{"jsonrpc":"2.0","method":"initialize","id":1,"id":2}\n',
                                  b"x" * (MAX_REQUEST_BYTES + 2) + b"\n",
                                  frame("tools/list", 3))
        self.assertEqual([reply["error"]["code"] for reply in replies],
                         [-32700, -32600, -32000])
        self.assertEqual(self.ensures, [])
        self.assertEqual(FakeClient.instances, [])

    def test_failed_request_not_replayed_next_explicit_request_reconnects(self):
        FakeClient.fail_next = True
        replies = self.run_frames(frame("initialize", 1),
                                  frame("tools/call", 2, {"name": "capture"}),
                                  frame("tools/list", 3))
        self.assertIn("not retried", replies[1]["error"]["message"])
        self.assertEqual(replies[2]["result"]["tools"][0]["name"], "capture")
        self.assertEqual(len(self.ensures), 2)
        self.assertEqual(sum(("tools/call", {"name": "capture"}) in c.calls
                             for c in FakeClient.instances), 1)

    def test_episode_is_relayed_without_action_or_database_rewriting(self):
        params = {"name": "episode", "arguments": {"db": "project", "action": "list", "limit": 3}}
        replies = self.run_frames(frame("initialize", 1), frame("tools/call", 2, params))
        self.assertFalse(replies[1]["result"]["isError"])
        self.assertIn(("tools/call", params), FakeClient.instances[0].calls)

    def test_failed_episode_write_is_not_replayed(self):
        FakeClient.fail_next = True
        params = {"name": "episode", "arguments": {
            "db": "project", "action": "append", "summary": "A selected scene",
            "source": {"namespace": "test", "key": "scene", "reference": "fixture://scene"}}}
        replies = self.run_frames(frame("initialize", 1), frame("tools/call", 2, params),
                                  frame("tools/list", 3))
        self.assertIn("not retried", replies[1]["error"]["message"])
        self.assertEqual(sum(("tools/call", params) in client.calls
                             for client in FakeClient.instances), 1)

    def test_invalid_initialize_does_not_ensure(self):
        replies = self.run_frames(frame("initialize", 1, {"protocolVersion": "1999-01-01"}))
        self.assertIn("unsupported MCP", replies[0]["error"]["message"])
        self.assertEqual(self.ensures, [])
        self.assertEqual(FakeClient.instances, [])

    def test_ensure_refusal_blocks_client_creation(self):
        def refuse(_):
            raise ValueError("missing project store")
        replies = self.run_frames(frame("initialize", 1), ensure=refuse)
        self.assertIn("missing project store", replies[0]["error"]["message"])
        self.assertEqual(FakeClient.instances, [])


if __name__ == "__main__":
    unittest.main()

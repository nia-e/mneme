"""Finite local profile tool tests; no Signal account or network."""

import io
import json
from pathlib import Path
import socket
import tempfile
import threading
import unittest

import signal_tools as profile


INIT = {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
    "protocolVersion": "2025-11-25", "capabilities": {},
    "clientInfo": {"name": "profile-test", "version": "1"}}}


def frame(value):
    return (json.dumps(value) + "\n").encode()


class Actor:
    def __init__(self, outcome="accepted"):
        self.calls = []
        self.outcome = outcome

    def update_profile(self, *, given_name=None, avatar=None):
        image = Path(avatar).read_bytes() if avatar is not None else None
        self.calls.append((given_name, image, avatar))
        return {"outcome": self.outcome}


class ProfileTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()
        self.state = self.root / "state"
        self.state.mkdir(mode=0o700)
        self.assets = self.root / "assets"
        self.assets.mkdir()
        self.portrait = self.assets / "portrait.png"
        self.portrait.write_bytes(profile.PNG + b"portrait")
        self.avatar = self.assets / "new.png"
        self.avatar.write_bytes(profile.PNG + b"new")
        self.socket_path = self.state / "profile.sock"

    def server(self):
        return profile.SignalToolsServer(self.socket_path, asset_roots=(self.assets,),
                                     portrait_path=self.portrait)

    def transact(self, server, actor, arguments, *, paused=False):
        worker = threading.Thread(target=lambda: server.poll(actor, paused=paused))
        worker.start()
        try:
            return profile._call_socket(self.socket_path, arguments)
        finally:
            worker.join(timeout=3)
            self.assertFalse(worker.is_alive())

    def test_catalog_mcp_framing_and_unavailable_bridge(self):
        request = {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {"_meta": {"progressToken": 1}}}
        call = {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "name": profile.PROFILE_TOOL, "arguments": {"given_name": "Example Agent"}}}
        sink = io.BytesIO()
        profile.run_stream(self.socket_path, io.BytesIO(frame(INIT) + frame(request) + frame(call)), sink)
        rows = [json.loads(line) for line in sink.getvalue().splitlines()]
        self.assertEqual(rows[0]["result"]["protocolVersion"], "2025-11-25")
        self.assertEqual([tool["name"] for tool in rows[1]["result"]["tools"]],
                         [profile.PROFILE_TOOL, profile.SEND_TOOL, profile.HISTORY_TOOL])
        self.assertEqual(rows[2]["result"]["structuredContent"]["outcome"], "failed")
        oversized = b"{" + b" " * (profile.MAX_FRAME + 3) + b"\n"
        sink = io.BytesIO()
        profile.run_stream(self.socket_path, io.BytesIO(oversized + frame(INIT)), sink)
        rows = [json.loads(line) for line in sink.getvalue().splitlines()]
        self.assertEqual(len(rows), 2)
        self.assertEqual(rows[0]["error"]["code"], -32700)
        self.assertEqual(rows[1]["result"]["protocolVersion"], "2025-11-25")

    def test_name_avatar_restore_and_private_stage(self):
        actor = Actor()
        with self.server() as server:
            self.assertEqual(self.socket_path.stat().st_mode & 0o777, 0o600)
            first = self.transact(server, actor, {"given_name": "Example Agent", "avatar_path": str(self.avatar)})
            self.assertEqual(first["outcome"], "accepted")
            self.assertEqual(actor.calls[0][:2], ("Example Agent", self.avatar.read_bytes()))
            self.assertEqual(actor.calls[0][2].parent, self.state)
            self.assertEqual(actor.calls[0][2].suffix, ".png")
            self.assertFalse(actor.calls[0][2].exists())
            second = self.transact(server, actor, {"restore_portrait": True})
            self.assertEqual(second["outcome"], "accepted")
            self.assertEqual(actor.calls[1][1], self.portrait.read_bytes())
            self.assertEqual(list(self.state.glob(".profile-*")), [])
        self.assertFalse(self.socket_path.exists())

    def test_pause_and_rejections_do_not_reach_actor(self):
        actor = Actor()
        outside = self.root / "outside.jpg"
        outside.write_bytes(b"\xff\xd8\xffoutside")
        bad = self.assets / "bad.png"
        bad.write_text("not an image")
        symlink = self.assets / "link.png"
        symlink.symlink_to(self.avatar)
        with self.server() as server:
            for args in ({"given_name": "Example Agent"},):
                self.assertEqual(self.transact(server, actor, args, paused=True)["reason"], "workshop_paused")
            for path in (outside, bad, symlink):
                self.assertEqual(self.transact(server, actor, {"avatar_path": str(path)})["outcome"], "failed")
            self.assertEqual(actor.calls, [])
            with self.assertRaisesRegex(ValueError, "conflicting_avatar_selection"):
                profile._call_socket(self.socket_path, {"avatar_path": str(self.avatar),
                                                        "restore_portrait": True})

    def test_unknown_actor_result_and_stale_socket(self):
        actor = Actor("unknown")
        with self.server() as server:
            self.assertEqual(self.transact(server, actor, {"given_name": "Example Agent"})["outcome"], "unknown")
            class BrokenActor:
                def update_profile(self, **_kwargs):
                    return {"outcome": "not-a-valid-result"}
            malformed = self.transact(server, BrokenActor(), {"given_name": "Example Agent"})
            self.assertEqual(malformed["outcome"], "unknown")
            self.assertEqual(malformed["reason"], "profile_result_unavailable")
        stale = socket.socket(socket.AF_UNIX)
        stale.bind(str(self.socket_path))
        stale.close()
        with self.server() as server:
            self.assertIsNotNone(server.listener)

    def test_stdio_mcp_to_bridge_actor_and_lost_ack(self):
        actor = Actor()
        call = {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": profile.PROFILE_TOOL, "arguments": {"given_name": "Example Agent", "avatar_path": str(self.avatar)}}}
        with self.server() as server:
            worker = threading.Thread(target=lambda: server.poll(actor))
            worker.start()
            sink = io.BytesIO()
            profile.run_stream(self.socket_path, io.BytesIO(frame(INIT) + frame(call)), sink)
            worker.join(timeout=3)
            self.assertFalse(worker.is_alive())
            result = [json.loads(line) for line in sink.getvalue().splitlines()][1]
            self.assertEqual(result["result"]["structuredContent"]["outcome"], "accepted")
            self.assertEqual(actor.calls[0][:2], ("Example Agent", self.avatar.read_bytes()))

        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(self.socket_path))
        listener.listen(1)
        def lose_ack():
            client, _ = listener.accept()
            with client:
                client.recv(4096)
            listener.close()
        worker = threading.Thread(target=lose_ack)
        worker.start()
        try:
            result = profile._call_socket(self.socket_path, {"given_name": "Example Agent"})
            self.assertEqual(result["outcome"], "unknown")
        finally:
            worker.join(timeout=3)
            self.socket_path.unlink(missing_ok=True)

    def test_active_socket_and_non_socket_not_removed(self):
        with self.server():
            with self.assertRaises(profile.SignalToolsError):
                with self.server():
                    pass
            self.assertTrue(self.socket_path.is_socket())
        self.socket_path.write_text("valuable")
        with self.assertRaises(profile.SignalToolsError):
            with self.server():
                pass
        self.assertEqual(self.socket_path.read_text(), "valuable")

    def test_send_message_dispatch_preserves_durable_key(self):
        actor = Actor()
        calls = []
        def send(arguments):
            calls.append(arguments.copy())
            return {"outcome": "accepted", "message_id": "local-7"}
        args = {"text": "hello", "request_id": "user:retry-1", "reply_to": "incoming.4"}
        with self.server() as server:
            worker = threading.Thread(target=lambda: server.poll(actor, send_message=send))
            worker.start()
            result = profile._call_socket(self.socket_path, args, tool=profile.SEND_TOOL)
            worker.join(timeout=3)
            self.assertFalse(worker.is_alive())
        self.assertEqual(result["outcome"], "accepted")
        self.assertEqual(result["message_id"], "local-7")
        self.assertEqual(calls, [args])
        self.assertNotEqual(result["request_id"], args["request_id"])
        self.assertEqual(actor.calls, [])

    def test_send_message_mcp_and_validation(self):
        actor = Actor()
        calls = []
        def send(arguments):
            calls.append(arguments)
            return {"outcome": "pending"}
        call = {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": profile.SEND_TOOL, "arguments": {"text": "hello", "request_id": "retry-1"}}}
        with self.server() as server:
            worker = threading.Thread(target=lambda: server.poll(actor, send_message=send))
            worker.start()
            sink = io.BytesIO()
            profile.run_stream(self.socket_path, io.BytesIO(frame(INIT) + frame(call)), sink)
            worker.join(timeout=3)
            self.assertFalse(worker.is_alive())
        rows = [json.loads(line) for line in sink.getvalue().splitlines()]
        self.assertEqual(rows[1]["result"]["structuredContent"]["outcome"], "pending")
        self.assertEqual(calls, [{"text": "hello", "request_id": "retry-1"}])
        for args in ({"text": "x"}, {"text": "", "request_id": "r"},
                     {"text": "x" * 4001, "request_id": "r"},
                     {"text": "x", "request_id": "bad key"},
                     {"text": "x", "request_id": "r", "reply_to": "?"},
                     {"text": "x", "request_id": "r", "recipient": "elsewhere"}):
            with self.assertRaises(ValueError):
                profile._call_socket(self.socket_path, args, tool=profile.SEND_TOOL)

    def test_send_paused_delegates_durable_status_and_lost_result_is_unknown(self):
        actor = Actor()
        calls = []
        prior = {"accepted-key": "accepted", "unknown-key": "unknown"}
        def send(arguments):
            calls.append(arguments.copy())
            outcome = prior.get(arguments["request_id"])
            return ({"outcome": outcome} if outcome else
                    {"outcome": "pending", "reason": "workshop_paused"})
        with self.server() as server:
            for key, expected in (("accepted-key", "accepted"), ("unknown-key", "unknown"),
                                  ("new-key", "pending")):
                worker = threading.Thread(target=lambda: server.poll(actor, paused=True,
                                                                   send_message=send))
                worker.start()
                result = profile._call_socket(self.socket_path, {"text": "x", "request_id": key},
                                              tool=profile.SEND_TOOL)
                worker.join(timeout=3)
                self.assertFalse(worker.is_alive())
                self.assertEqual(result["outcome"], expected)
                if expected == "pending":
                    self.assertEqual(result["reason"], "workshop_paused")
            self.assertEqual(actor.calls, [])
            self.assertEqual([call["request_id"] for call in calls],
                             ["accepted-key", "unknown-key", "new-key"])
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(self.socket_path))
        listener.listen(1)
        def lose_ack():
            client, _ = listener.accept()
            with client:
                client.recv(4096)
            listener.close()
        worker = threading.Thread(target=lose_ack)
        worker.start()
        try:
            result = profile._call_socket(self.socket_path, {"text": "x", "request_id": "same-key"},
                                          tool=profile.SEND_TOOL)
            self.assertEqual(result["outcome"], "unknown")
        finally:
            worker.join(timeout=3)
            self.socket_path.unlink(missing_ok=True)

    def test_send_retry_reuses_durable_id_and_invalid_callback_is_unknown(self):
        actor = Actor()
        seen = []
        args = {"text": "same text", "request_id": "same-key"}
        with self.server() as server:
            for outcome in ("unknown", "superseded"):
                def send(arguments):
                    seen.append(arguments.copy())
                    return {"outcome": outcome}
                worker = threading.Thread(target=lambda: server.poll(actor, send_message=send))
                worker.start()
                result = profile._call_socket(self.socket_path, args, tool=profile.SEND_TOOL)
                worker.join(timeout=3)
                self.assertEqual(result["outcome"], outcome)
            worker = threading.Thread(target=lambda: server.poll(
                actor, send_message=lambda _args: {"outcome": "not-an-outcome"}))
            worker.start()
            result = profile._call_socket(self.socket_path, args, tool=profile.SEND_TOOL)
            worker.join(timeout=3)
            self.assertEqual(result["outcome"], "unknown")
            self.assertEqual(result["reason"], "result_unavailable")
        self.assertEqual(seen, [args, args])

    def test_read_history_catalog_dispatch_bounds_and_pause(self):
        catalog = next(tool for tool in profile.CATALOG if tool["name"] == profile.HISTORY_TOOL)
        self.assertFalse(catalog["annotations"]["readOnlyHint"])
        self.assertIn("does not mark them answered", catalog["description"])
        actor = Actor()
        reads = []
        history = {"entries": [{"direction": "outgoing", "status": "accepted",
                                 "text": "prior", "timestamp_ms": 17, "id": "sent-1"}],
                   "uncertain": [], "truncated": False}
        def read(arguments):
            reads.append(arguments.copy())
            return history
        with self.server() as server:
            for arguments, limit in (({}, 12), ({"limit": 3}, 3)):
                worker = threading.Thread(target=lambda: server.poll(
                    actor, paused=True, read_history=read,
                    send_message=lambda _args: self.fail("history must not send")))
                worker.start()
                result = profile._call_socket(self.socket_path, arguments, tool=profile.HISTORY_TOOL)
                worker.join(timeout=3)
                self.assertFalse(worker.is_alive())
                self.assertEqual({key: value for key, value in result.items() if key != "request_id"}, history)
                self.assertEqual(reads[-1], {"limit": limit})
            self.assertEqual(actor.calls, [])
        for arguments in ({"limit": 0}, {"limit": 21}, {"limit": True},
                          {"limit": 1.5}, {"limit": "12"}, {"recipient": "other"}):
            with self.assertRaises(ValueError):
                profile._call_socket(self.socket_path, arguments, tool=profile.HISTORY_TOOL)

    def test_read_history_mcp_and_lost_socket_result_is_unknown(self):
        actor = Actor()
        history = {"entries": [], "uncertain": [{"direction": "outgoing", "status": "unknown",
                                                   "text": "maybe", "timestamp_ms": None, "id": "u-1"}],
                   "truncated": False}
        call = {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": profile.HISTORY_TOOL, "arguments": {"limit": 1}}}
        with self.server() as server:
            worker = threading.Thread(target=lambda: server.poll(actor, read_history=lambda _args: history))
            worker.start()
            sink = io.BytesIO()
            profile.run_stream(self.socket_path, io.BytesIO(frame(INIT) + frame(call)), sink)
            worker.join(timeout=3)
        rows = [json.loads(line) for line in sink.getvalue().splitlines()]
        value = rows[1]["result"]["structuredContent"]
        self.assertEqual(value["uncertain"], history["uncertain"])
        self.assertNotIn("outcome", value)
        missing = profile._call_socket(self.socket_path, {}, tool=profile.HISTORY_TOOL)
        self.assertEqual(missing["outcome"], "failed")
        self.assertEqual(missing["reason"], "tools_bridge_unavailable")
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(self.socket_path))
        listener.listen(1)
        def lose_result():
            client, _ = listener.accept()
            with client:
                client.recv(4096)
            listener.close()
        worker = threading.Thread(target=lose_result)
        worker.start()
        try:
            lost = profile._call_socket(self.socket_path, {}, tool=profile.HISTORY_TOOL)
            self.assertEqual(lost["outcome"], "unknown")
            self.assertEqual(lost["reason"], "result_unavailable")
        finally:
            worker.join(timeout=3)
            self.socket_path.unlink(missing_ok=True)

    def test_oversized_history_callback_has_unknown_mark_seen_outcome(self):
        too_large = {"entries": [{"text": "x" * 4100}], "uncertain": [], "truncated": True}
        with self.server() as server:
            worker = threading.Thread(target=lambda: server.poll(
                Actor(), read_history=lambda _args: too_large))
            worker.start()
            result = profile._call_socket(self.socket_path, {}, tool=profile.HISTORY_TOOL)
            worker.join(timeout=3)
        self.assertEqual(result["outcome"], "unknown")
        self.assertEqual(result["reason"], "result_unavailable")

    def test_history_callback_failure_after_invocation_is_unknown(self):
        for error in (OSError("post-commit write lost"), ValueError("post-commit queue refused"),
                      RuntimeError("post-commit crash")):
            with self.subTest(error=type(error).__name__), self.server() as server:
                def read(_args):
                    raise error
                worker = threading.Thread(target=lambda: server.poll(Actor(), read_history=read))
                worker.start()
                result = profile._call_socket(self.socket_path, {}, tool=profile.HISTORY_TOOL)
                worker.join(timeout=3)
                self.assertFalse(worker.is_alive())
                self.assertEqual(result["outcome"], "unknown")
                self.assertEqual(result["reason"], "result_unavailable")

    def test_mcp_socket_path_must_be_absolute_canonical_private_parent(self):
        with self.assertRaises(profile.SignalToolsError):
            profile.MCPServer(Path("relative.sock"))
        public = self.root / "public"
        public.mkdir(mode=0o755)
        public.chmod(0o755)  # Explicit despite an operator's private process umask.
        with self.assertRaises(profile.SignalToolsError):
            profile.MCPServer(public / "profile.sock")
        alias = self.root / "alias"
        alias.symlink_to(self.state)
        with self.assertRaises(profile.SignalToolsError):
            profile.MCPServer(alias / "profile.sock")


if __name__ == "__main__":
    unittest.main()

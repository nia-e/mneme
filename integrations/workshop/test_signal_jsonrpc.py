"""No-network transport tests; every child is a synthetic local executable."""

import json
import os
from pathlib import Path
import signal
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

import signal_jsonrpc as rpc


ACCOUNT = "11111111-1111-4111-8111-111111111111"
OWNER = "22222222-2222-4222-8222-222222222222"
OTHER = "33333333-3333-4333-8333-333333333333"

FAKE = r'''
import json, os, signal, sys, time
from pathlib import Path
root = Path(__file__).parent
(root / "launch.json").write_text(json.dumps({"argv": sys.argv[1:], "env": dict(os.environ), "pid": os.getpid()}))
if mode == "ignore_term":
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
def emit(value):
    print(json.dumps(value, ensure_ascii=False), flush=True)
def notification(number):
    return {"jsonrpc": "2.0", "method": "receive", "params": {"subscription": 7,
        "result": {"account": account, "envelope": {"sourceUuid": owner,
        "sourceDevice": 1, "timestamp": number, "dataMessage": {"timestamp": number, "message": "hi 🌿"}}}}}
for line in sys.stdin:
    request = json.loads(line)
    with (root / "requests.jsonl").open("a") as log:
        log.write(json.dumps(request) + "\n")
    if request["method"] == "subscribeReceive":
        if mode == "start_timeout":
            time.sleep(60)
        if mode == "malformed":
            print("secret not JSON", flush=True)
            time.sleep(60)
        if mode == "oversize":
            sys.stdout.write("x" * (256 * 1024 + 1)); sys.stdout.flush()
            time.sleep(60)
        if mode == "truncated":
            sys.stdout.write('{"jsonrpc":'); sys.stdout.flush()
            break
        if mode == "early":
            emit(notification(1))
        if mode == "refused":
            emit({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -1}})
        else:
            emit({"jsonrpc": "2.0", "id": request["id"], "result": 7})
        if mode == "many":
            time.sleep(.05)
            for n in range(300):
                emit(notification(n))
        if mode == "fragmented":
            time.sleep(.05)
            raw = (json.dumps(notification(10), ensure_ascii=False) + "\n").encode()
            cut = raw.index("🌿".encode()) + 1
            os.write(1, raw[:cut]); time.sleep(.03); os.write(1, raw[cut:])
        if mode == "wrong_id":
            time.sleep(.05)
            emit({"jsonrpc": "2.0", "id": "unrelated", "result": 1})
    elif request["method"] == "send":
        if mode in ("early", "callback"):
            emit(notification(2))
        if mode == "send_timeout":
            time.sleep(60)
        if mode == "disconnect":
            break
        if mode == "rpc_rejected":
            emit({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32602}})
            continue
        kind = "NETWORK_FAILURE" if mode == "network" else "UNREGISTERED_FAILURE" if mode == "recipient_refused" else "SUCCESS"
        target = other if mode == "wrong_recipient" else request["params"]["recipient"][0]
        body = {"timestamp": 123456789, "results": [{"recipientAddress": {"uuid": target}, "type": kind}]}
        if mode == "missing_timestamp":
            del body["timestamp"]
        emit({"jsonrpc": "2.0", "id": request["id"], "result": body})
    elif request["method"] == "updateProfile":
        if mode in ("profile_interleaved", "callback"):
            emit(notification(3))
        if mode == "profile_timeout":
            time.sleep(60)
        if mode == "profile_disconnect":
            break
        if mode == "profile_error":
            emit({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -3, "message": "private file detail"}})
            continue
        if mode == "profile_wrong_id":
            emit({"jsonrpc": "2.0", "id": "not-this-request", "result": {}})
            continue
        result = {"unexpected": True} if mode == "profile_nonempty" else None if mode == "profile_null" else {}
        emit({"jsonrpc": "2.0", "id": request["id"], "result": result})
'''


class SignalRpcTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()
        self.cli = self.root / "fake-signal-cli"
        self.data = self.root / "data"
        self.data.mkdir(mode=0o700)
        self.config = self.root / "config.json"
        self.config.write_text(json.dumps(rpc.MINIMAL_CONFIG))
        self.config.chmod(0o600)
        self.received = []

    def transport(self, mode="good", **kwargs):
        self.cli.write_text(f"#!{sys.executable}\nmode={mode!r}\naccount={ACCOUNT!r}\nowner={OWNER!r}\nother={OTHER!r}\n" + FAKE)
        self.cli.chmod(0o700)
        obj = rpc.SignalRpc(self.cli, ACCOUNT, self.data, self.config,
                            kwargs.pop("on_receive", self.received.append),
                            start_timeout=kwargs.pop("start_timeout", 2),
                            send_timeout=kwargs.pop("send_timeout", .3),
                            close_timeout=.1, **kwargs)
        self.addCleanup(obj.close)
        return obj

    def requests(self):
        return [json.loads(line) for line in (self.root / "requests.jsonl").read_text().splitlines()]

    def assert_stopped(self, process):
        self.assertIsNotNone(process.poll())
        self.assertTrue(process.stdin.closed)
        self.assertTrue(process.stdout.closed)

    def test_early_notification_and_interleaved_send(self):
        obj = self.transport("early").start()
        self.assertEqual(obj.subscription, 7)
        self.assertEqual(len(self.received), 1)
        self.assertEqual(self.received[0]["params"]["result"]["account"], ACCOUNT)
        self.assertEqual(obj.send(OWNER, "hello"), {"outcome": "accepted", "timestamp_ms": 123456789})
        self.assertEqual(len(self.received), 2)
        requests = self.requests()
        self.assertEqual([r["method"] for r in requests], ["subscribeReceive", "send"])
        self.assertEqual(requests[1]["params"], {"recipient": [OWNER], "message": "hello"})
        self.assertNotEqual(requests[0]["id"], requests[1]["id"])

    def test_argv_and_environment_are_explicit(self):
        with patch.dict(os.environ, {"JAVA_TOOL_OPTIONS": "-Dsecret=must-not-inherit", "SIGNAL_CLI_CONFIG": "/wrong", "SECRET": "private"}):
            self.transport().start()
        launch = json.loads((self.root / "launch.json").read_text())
        self.assertEqual(launch["argv"], ["--data-dir", str(self.data), "--log-file", "/dev/null", "--scrub-log",
            "-a", ACCOUNT, "-o", "json", "jsonRpc", "--receive-mode", "manual", "--ignore-attachments",
            "--ignore-avatars", "--ignore-stickers", "--ignore-stories"])
        self.assertNotIn("JAVA_TOOL_OPTIONS", launch["env"])
        self.assertNotIn("SECRET", launch["env"])
        self.assertEqual(launch["env"]["SIGNAL_CLI_CONFIG"], str(self.config))
        self.assertEqual(launch["env"]["PATH"], "/usr/bin:/bin")

    def test_config_refuses_before_spawn_and_does_not_mutate(self):
        obj = self.transport()
        self.config.chmod(0o644)
        before = self.config.read_bytes()
        with patch.object(rpc.subprocess, "Popen") as spawn:
            with self.assertRaisesRegex(rpc.TransportError, "unsafe_config_file"):
                obj.start()
            spawn.assert_not_called()
        self.assertEqual(self.config.read_bytes(), before)
        self.config.chmod(0o600)
        self.config.write_text(json.dumps(dict(rpc.MINIMAL_CONFIG, verbose=1)))
        with self.assertRaisesRegex(rpc.TransportError, "nonminimal_cli_config"):
            rpc.validate_config(self.config)
        self.config.write_text(json.dumps(rpc.MINIMAL_CONFIG))
        linked = self.root / "linked.json"
        linked.symlink_to(self.config)
        with self.assertRaisesRegex(rpc.TransportError, "invalid_config_path"):
            rpc.validate_config(linked)

    def test_ambient_system_config_refuses_before_spawn(self):
        obj = self.transport()
        real_exists = Path.exists
        def exists(path):
            return True if str(path) == "/etc/signal-cli/config.json" else real_exists(path)
        with patch.object(Path, "exists", exists), patch.object(rpc.subprocess, "Popen") as spawn:
            with self.assertRaisesRegex(rpc.TransportError, "ambient_system_config"):
                obj.start()
            spawn.assert_not_called()

    def test_start_failure_diagnostics_and_cleanup(self):
        for mode, code in [("malformed", "invalid_json"), ("oversize", "frame_too_large"),
                           ("truncated", "truncated_frame"), ("refused", "subscription_refused"),
                           ("start_timeout", "request_timeout")]:
            with self.subTest(mode=mode):
                obj = self.transport(mode, start_timeout=.5)
                with self.assertRaises(rpc.TransportError) as caught:
                    obj.start()
                self.assertEqual(str(caught.exception), code)
                self.assertTrue(obj._closed)
                self.assertIsNone(obj._process)
                with self.assertRaisesRegex(rpc.TransportError, "transport_closed_or_busy"):
                    obj.start()

    def test_callback_failure_stops_child_without_payload_diagnostic(self):
        def reject(_):
            raise ValueError("private body must never be logged")
        obj = self.transport("callback", on_receive=reject).start()
        process = obj._process
        self.assertEqual(obj.send(OWNER, "hello"), {"outcome": "unknown", "reason": "receive_callback_failed"})
        self.assert_stopped(process)
        self.assertEqual(len([r for r in self.requests() if r["method"] == "send"]), 1)

    def test_callback_failure_before_subscription_response_is_not_dropped(self):
        seen = []
        def reject(frame):
            seen.append(frame)
            raise RuntimeError("private")
        obj = self.transport("early", on_receive=reject)
        with self.assertRaisesRegex(rpc.TransportError, "receive_callback_failed"):
            obj.start()
        self.assertEqual(len(seen), 1)
        self.assertTrue(obj._closed)

    def test_timeout_or_disconnect_after_write_is_unknown_and_never_retried(self):
        for mode in ("send_timeout", "disconnect"):
            with self.subTest(mode=mode):
                (self.root / "requests.jsonl").unlink(missing_ok=True)
                obj = self.transport(mode, send_timeout=.15).start()
                process = obj._process
                result = obj.send(OWNER, "hello")
                self.assertEqual(result["outcome"], "unknown")
                self.assertEqual(len([r for r in self.requests() if r["method"] == "send"]), 1)
                self.assertEqual(obj.send(OWNER, "hello")["outcome"], "failed")
                self.assert_stopped(process)

    def test_zero_send_deadline_is_known_no_send(self):
        obj = self.transport(send_timeout=0).start()
        self.assertEqual(obj.send(OWNER, "hello"), {"outcome": "failed", "reason": "request_timeout"})
        self.assertEqual([r["method"] for r in self.requests()], ["subscribeReceive"])

    def test_exact_recipient_and_positive_matching_result_required(self):
        for mode, expected in [("good", "accepted"), ("wrong_recipient", "unknown"),
                               ("missing_timestamp", "unknown"), ("network", "unknown"),
                               ("recipient_refused", "failed"), ("rpc_rejected", "failed")]:
            with self.subTest(mode=mode):
                with self.transport(mode) as obj:
                    self.assertEqual(obj.send(OWNER, "hello")["outcome"], expected)

    def test_result_errors_never_leak_payload(self):
        body = {"timestamp": 42, "results": [{"recipientAddress": {"uuid": OWNER, "number": "private"}, "type": "NETWORK_FAILURE"}]}
        result = rpc.SignalRpc._send_result({"error": {"code": -1, "message": "private", "data": {"response": body}}}, OWNER)
        self.assertEqual(result, {"outcome": "unknown", "reason": "send_outcome_unconfirmed"})
        body["results"][0]["type"] = "IDENTITY_FAILURE"
        self.assertEqual(rpc.SignalRpc._send_result({"error": {"data": {"response": body}}}, OWNER)["outcome"], "failed")
        body["results"][0]["type"] = "SUCCESS"
        self.assertEqual(rpc.SignalRpc._send_result({"error": {"data": {"response": body}}}, OWNER)["outcome"], "unknown")

    def test_invalid_inputs_and_fixed_recipient(self):
        obj = self.transport().start()
        for message in ("", "x" * 4001, None, "\ud800"):
            with self.subTest(message_type=type(message).__name__):
                with self.assertRaises(rpc.TransportError):
                    obj.send(OWNER, message)
        with self.assertRaisesRegex(rpc.TransportError, "invalid_account_identifier"):
            obj.send("user.username", "hello")
        self.assertEqual(obj.send(OWNER, "hello")["outcome"], "accepted")
        with self.assertRaisesRegex(rpc.TransportError, "recipient_binding_changed"):
            obj.send(OTHER, "hello")
        self.assertEqual(len([r for r in self.requests() if r["method"] == "send"]), 1)

    def test_fragmented_utf8_notification(self):
        obj = self.transport("fragmented").start()
        self.assertEqual(obj.poll(.5), 1)
        self.assertEqual(self.received[0]["params"]["result"]["envelope"]["dataMessage"]["message"], "hi 🌿")

    def test_poll_is_bounded_and_retains_remaining_frames(self):
        obj = self.transport("many").start()
        counts = []
        deadline = time.monotonic() + 3
        while len(self.received) < 300 and time.monotonic() < deadline:
            counts.append(obj.poll(.2))
        self.assertEqual(len(self.received), 300)
        self.assertTrue(all(n <= rpc.MAX_POLL_FRAMES for n in counts))
        self.assertEqual([f["params"]["result"]["envelope"]["timestamp"] for f in self.received], list(range(300)))
        self.assertTrue(obj.caught_up)

    def test_caught_up_refuses_buffered_partial_or_complete_frame(self):
        obj = self.transport().start()
        self.assertTrue(obj.caught_up)
        obj._buffer.extend(b'{"jsonrpc":')
        self.assertFalse(obj.caught_up)
        self.assertEqual(obj.poll(0), 0)
        self.assertFalse(obj.caught_up)
        obj._buffer.clear()
        frame = {"jsonrpc": "2.0", "method": "receive", "params": {}}
        obj._buffer.extend((json.dumps(frame).encode() + b"\n") * (rpc.MAX_POLL_FRAMES + 1))
        self.assertEqual(obj.poll(0), rpc.MAX_POLL_FRAMES)
        self.assertFalse(obj.caught_up)
        self.assertEqual(obj.poll(0), 1)
        self.assertTrue(obj.caught_up)

    def test_unknown_response_refuses_and_closes(self):
        obj = self.transport("wrong_id").start()
        process = obj._process
        with self.assertRaisesRegex(rpc.TransportError, "unexpected_rpc_frame"):
            obj.poll(.5)
        self.assert_stopped(process)

    def test_decode_bounds_and_duplicate_keys(self):
        for raw, code in [(b'{"a":1,"a":2}', "duplicate_json_key"),
                          (b"[" * 33 + b"]" * 33, "json_too_deep"),
                          (b'{"x":NaN}', "invalid_json"), (b"\xff", "invalid_json")]:
            with self.subTest(code=code), self.assertRaisesRegex(rpc.TransportError, code):
                rpc._decode(raw)
        self.assertEqual(rpc._decode(b'{"text":"[[\\\"[["}'), {"text": '[["[['})

    def test_close_kills_term_ignoring_child_and_is_idempotent(self):
        obj = self.transport("ignore_term").start()
        process = obj._process
        started = time.monotonic()
        obj.close()
        self.assertLess(time.monotonic() - started, 1)
        self.assert_stopped(process)
        self.assertEqual(process.returncode, -signal.SIGKILL)
        obj.close()

    def test_interrupt_during_callback_closes_process(self):
        def interrupt(_):
            raise KeyboardInterrupt
        obj = self.transport("callback", on_receive=interrupt).start()
        process = obj._process
        with self.assertRaises(KeyboardInterrupt):
            obj.send(OWNER, "hello")
        self.assert_stopped(process)

    def test_profile_interleaves_receive_and_preserves_send_behavior(self):
        avatar = self.root / "staged.png"
        avatar.write_bytes(b"synthetic image; format admission belongs to ProfileServer")
        avatar.chmod(0o600)
        obj = self.transport("profile_interleaved").start()
        self.assertEqual(obj.update_profile(avatar=avatar), {"outcome": "accepted"})
        self.assertEqual(len(self.received), 1)
        self.assertEqual(obj.update_profile(given_name="Example Agent", avatar=avatar), {"outcome": "accepted"})
        self.assertEqual(len(self.received), 2)
        self.assertEqual(obj.send(OWNER, "hello"), {"outcome": "accepted", "timestamp_ms": 123456789})
        self.assertEqual(obj.update_profile(given_name="Example Agent 🌿"), {"outcome": "accepted"})
        requests = self.requests()
        self.assertEqual([r["method"] for r in requests], ["subscribeReceive", "updateProfile", "updateProfile", "send", "updateProfile"])
        self.assertEqual(requests[1]["params"], {"avatar": str(avatar)})
        self.assertEqual(requests[2]["params"], {"givenName": "Example Agent", "avatar": str(avatar)})
        self.assertEqual(requests[4]["params"], {"givenName": "Example Agent 🌿"})
        self.assertEqual(len({r["id"] for r in requests}), len(requests))

    def test_profile_invalid_inputs_are_known_failures_before_rpc(self):
        obj = self.transport().start()
        public_file = self.root / "public.png"
        public_file.write_bytes(b"image")
        public_file.chmod(0o644)
        private_file = self.root / "private.png"
        private_file.write_bytes(b"image")
        private_file.chmod(0o600)
        link = self.root / "symlink.png"
        link.symlink_to(private_file)
        for arguments in ({}, {"given_name": ""}, {"given_name": "x" * 129}, {"given_name": True},
                          {"given_name": "\ud800"}, {"avatar": "data:image/png;base64,AA=="},
                          {"avatar": "relative.png"}, {"avatar": self.root / "missing.png"},
                          {"avatar": public_file}, {"avatar": link}, {"avatar": self.data}):
            with self.subTest(arguments=repr(arguments)):
                self.assertEqual(obj.update_profile(**arguments)["outcome"], "failed")
        self.assertEqual([r["method"] for r in self.requests()], ["subscribeReceive"])
        self.assertTrue(obj.caught_up)

    def test_profile_exact_result_required_and_errors_remain_unknown(self):
        for mode in ("profile_nonempty", "profile_null", "profile_error", "profile_wrong_id"):
            with self.subTest(mode=mode), self.transport(mode) as obj:
                result = obj.update_profile(given_name="Example Agent")
                self.assertEqual(result["outcome"], "unknown")
                self.assertNotIn("private", str(result))

    def test_profile_postwrite_loss_is_unknown_without_retry(self):
        for mode in ("profile_timeout", "profile_disconnect"):
            with self.subTest(mode=mode):
                (self.root / "requests.jsonl").unlink(missing_ok=True)
                obj = self.transport(mode, send_timeout=.15).start()
                process = obj._process
                self.assertEqual(obj.update_profile(given_name="Example Agent")["outcome"], "unknown")
                self.assertEqual(obj.update_profile(given_name="Example Agent")["outcome"], "failed")
                self.assertEqual(len([r for r in self.requests() if r["method"] == "updateProfile"]), 1)
                self.assert_stopped(process)

    def test_profile_prewrite_failure_is_known_no_change(self):
        obj = self.transport(send_timeout=0)
        self.assertEqual(obj.update_profile(given_name="Example Agent"), {"outcome": "failed", "reason": "transport_not_started"})
        obj.start()
        self.assertEqual(obj.update_profile(given_name="Example Agent"), {"outcome": "failed", "reason": "request_timeout"})
        self.assertEqual([r["method"] for r in self.requests()], ["subscribeReceive"])

    def test_profile_callback_failure_is_unknown_and_closes_child(self):
        def reject(_):
            raise RuntimeError("private callback detail")
        obj = self.transport("callback", on_receive=reject).start()
        process = obj._process
        self.assertEqual(obj.update_profile(given_name="Example Agent"), {"outcome": "unknown", "reason": "receive_callback_failed"})
        self.assert_stopped(process)


if __name__ == "__main__":
    unittest.main()

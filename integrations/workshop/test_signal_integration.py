"""One synthetic full path: real modules, fake local signal-cli and Codex only."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import heartbeat
import signal_bridge as bridge
import signal_jsonrpc
import signal_store
import wake


ACCOUNT = "11111111-1111-4111-8111-111111111111"
OWNER = "22222222-2222-4222-8222-222222222222"
ANSWER = "Six messages received; one answer."

FAKE_SIGNAL = r'''
import json
from pathlib import Path
import sys

root = Path(__file__).parent
account = "11111111-1111-4111-8111-111111111111"
owner = "22222222-2222-4222-8222-222222222222"
for line in sys.stdin:
    request = json.loads(line)
    with (root / "signal-requests.jsonl").open("a") as log:
        log.write(json.dumps(request) + "\n")
    if request["method"] == "subscribeReceive":
        for number in range(6):
            frame = {"jsonrpc": "2.0", "method": "receive", "params": {"subscription": 7,
                "result": {"account": account, "envelope": {"sourceUuid": owner,
                    "sourceDevice": 1, "timestamp": 1000 + number,
                    "dataMessage": {"timestamp": 1000 + number,
                                    "message": "message " + str(number)}}}}}
            print(json.dumps(frame), flush=True)
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": 7}), flush=True)
    elif request["method"] == "send":
        response = {"jsonrpc": "2.0", "id": request["id"], "result": {
            "timestamp": 123456789, "results": [{"recipientAddress": {"uuid": owner},
                "type": "SUCCESS"}]}}
        print(json.dumps(response), flush=True)
'''

FAKE_CODEX = r'''
import json
import os
from pathlib import Path
import sys

arguments = sys.argv[1:]
out = Path(arguments[arguments.index("--output-last-message") + 1])
event_id = os.environ.get("FAKE_EVENT_ID")
if out.parent.name == "orientation":
    value = {"status": "work" if event_id else "rest",
             "summary": "Read private chat." if event_id else "Baseline rest.",
             "task": "Respond using the configured Signal tool." if event_id else "",
             "effort": "brief", "next_wake_seconds": 3600, "replies": []}
else:
    value = {"status": "rest", "summary": "Signal tool response exercised separately.",
             "artifacts": [], "next_step": "", "memory_candidates": [],
             "next_wake_seconds": 3600, "replies": []}
out.write_text(json.dumps(value))
'''


class SignalIntegrationTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        self.workshop = self.base / "workshop"
        self.workshop.mkdir()
        (self.workshop / "AGENDA.md").write_text("# Workshop\nRest unless addressed.\n")
        self.state = self.base / "signal-state"
        self.data = self.base / "signal-data"
        self.data.mkdir(mode=0o700)
        self.cli = self.base / "fake-signal-cli"
        self.cli.write_text("#!" + sys.executable + "\n" + FAKE_SIGNAL)
        self.cli.chmod(0o700)
        self.codex = self.base / "fake-codex"
        self.codex.write_text("#!" + sys.executable + "\n" + FAKE_CODEX)
        self.codex.chmod(0o700)
        self.rpc_config = self.base / "signal-cli.json"
        self.rpc_config.write_text(json.dumps(signal_jsonrpc.MINIMAL_CONFIG))
        self.rpc_config.chmod(0o600)
        self.config_path = self.base / "bridge.json"
        self.config_path.write_text(json.dumps({
            "schema": bridge.CONFIG_SCHEMA, "workshop_root": str(self.workshop),
            "state_dir": str(self.state), "store_path": str(self.state / "ledger.sqlite"),
            "signal_cli": str(self.cli), "signal_data_dir": str(self.data),
            "signal_rpc_config": str(self.rpc_config), "account": ACCOUNT,
            "owner_id": OWNER}))
        self.config_path.chmod(0o600)
        self.config = bridge.Config(self.config_path)
        self.clock = [0.0]

    def runner(self, event_id=None):
        env = os.environ.copy()
        if event_id is None:
            env.pop("FAKE_EVENT_ID", None)
        else:
            env["FAKE_EVENT_ID"] = event_id
        process = subprocess.run(
            [sys.executable, str(Path(heartbeat.__file__).resolve()),
             "--root", str(self.workshop), "--codex", str(self.codex),
             "--signal-source", bridge.SOURCE, "--timeout", "6", "run"],
            env=env, capture_output=True, text=True, timeout=10)
        self.assertEqual(process.returncode, 0, (process.stdout, process.stderr))
        return json.loads(process.stdout)

    def open_pair(self):
        store = signal_store.Store(self.state / "ledger.sqlite", owner_id=OWNER)
        actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                              clock=lambda: self.clock[0])
        rpc = signal_jsonrpc.SignalRpc(self.cli, ACCOUNT, self.data, self.rpc_config,
                                       actor.receive, start_timeout=2, send_timeout=2,
                                       close_timeout=.2)
        return store, actor, rpc

    def test_six_received_one_interactive_reply_no_replay_send(self):
        # Establish a non-due background lane through the real runner first.
        baseline = self.runner()
        self.assertEqual(baseline["cycle_class"], "background")
        self.assertEqual(bridge.init(self.config)["status"], "initialized")

        store, actor, rpc = self.open_pair()
        with store, rpc:
            rpc.start()  # fake signal-cli emits six authenticated notifications
            self.assertEqual(store.status()["generation"], 6)
            self.assertEqual(store.status()["unanswered"], 6)
            self.assertEqual(store.pending_bursts(), [])
            self.clock[0] = 10
            actor.tick(rpc)
            queued = wake.active_records(self.workshop)
            self.assertEqual(len(queued), 1)
            burst = store.awaiting_replies()[0]
            self.assertEqual(burst["message_ids"], [f"{1000+i}:1" for i in range(6)])
            self.assertEqual(burst["body"].count("Owner: "), 6)
            self.assertEqual(queued[0]["event"]["body"], burst["body"])

            completed = self.runner(burst["event_id"])
            self.assertEqual(completed["status"], "completed")
            self.assertEqual(completed["cycle_class"], "interactive")
            run = self.workshop / "runs" / completed["run_id"]
            receipt = json.loads((run / "receipt.json").read_text())
            result = json.loads((run / "result.json").read_text())
            self.assertEqual(receipt["schema"], heartbeat.RECEIPT_SCHEMA)
            self.assertTrue(receipt["orientation_valid"])
            self.assertEqual(receipt["status"], "completed")
            self.assertEqual(receipt["event_ids"], [{"source": bridge.SOURCE,
                                                      "event_id": burst["event_id"]}])
            self.assertEqual(result["replies"], [])
            self.assertEqual(store.status()["unanswered"], 6)
            sent = actor.send_message(rpc, {"request_id": "direct-1", "text": ANSWER,
                                            "reply_to": burst["event_id"]})
            self.assertEqual(sent["outcome"], "accepted")
            actor.tick(rpc)
            self.assertEqual(store.status()["unanswered"], 0)
            self.assertEqual(store.status()["pending_outbox"], 0)

        # A new bridge/transport instance sees the same six upstream duplicates.
        # Durable message IDs and accepted outbox state suppress another turn/send.
        store, actor, rpc = self.open_pair()
        with store, rpc:
            store.recover_uncertain_sends()
            rpc.start()
            self.assertEqual(store.status()["generation"], 6)
            self.clock[0] = 20
            actor.tick(rpc)
            self.assertEqual(store.status()["bursts"], 1)
            self.assertEqual(store.status()["outbox"], 0)
            self.assertEqual(store.status()["direct_outbox"], 1)
            self.assertEqual(store.status()["unanswered"], 0)
        requests = [json.loads(line) for line in
                    (self.base / "signal-requests.jsonl").read_text().splitlines()]
        sends = [item for item in requests if item["method"] == "send"]
        self.assertEqual(len(sends), 1)
        self.assertEqual(sends[0]["params"], {"recipient": [OWNER], "message": ANSWER})
        self.assertEqual(len([item for item in requests if item["method"] == "subscribeReceive"]), 2)


if __name__ == "__main__":
    unittest.main()

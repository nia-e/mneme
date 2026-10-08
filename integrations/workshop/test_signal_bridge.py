"""Signal bridge composition tests: synthetic notifications, no account/model/network."""

import datetime
import fcntl
import json
import os
import sqlite3
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import signal_bridge as bridge
import signal_jsonrpc
import signal_store
import wake


ACCOUNT = "11111111-1111-4111-8111-111111111111"
OWNER = "22222222-2222-4222-8222-222222222222"


def notification(text="hello", *, account=ACCOUNT, owner=OWNER, timestamp=1000, device=1,
                 extras=None):
    data = {"timestamp": timestamp, "message": text}
    data.update(extras or {})
    return {"jsonrpc": "2.0", "method": "receive", "params": {"subscription": 7,
            "result": {"account": account, "envelope": {"sourceUuid": owner,
                       "sourceDevice": device, "timestamp": timestamp,
                       "dataMessage": data}}}}


class FakeRpc:
    def __init__(self, *, outcome="accepted", on_poll=None, caught_up=True):
        self.outcome = outcome
        self.on_poll = on_poll
        self.sent = []
        self.polls = 0
        self.caught_up = caught_up

    def poll(self, timeout=0):
        self.polls += 1
        if self.on_poll is not None:
            action, self.on_poll = self.on_poll, None
            action()
        return 0

    def send(self, owner, text):
        self.sent.append((owner, text))
        if self.outcome == "transport_error":
            raise signal_jsonrpc.TransportError("synthetic send loss")
        return {"outcome": self.outcome, "timestamp_ms": 1234 if self.outcome == "accepted" else None}


class BridgeTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.base = Path(tmp.name).resolve()
        self.workshop = self.base / "workshop"
        self.workshop.mkdir()
        self.data = self.base / "signal-data"
        self.data.mkdir()
        self.cli = self.base / "signal-cli"
        self.cli.write_text("#!/bin/sh\nexit 1\n")
        self.cli.chmod(0o700)
        self.rpc_config = self.base / "cli.json"
        self.rpc_config.write_text(json.dumps(signal_jsonrpc.MINIMAL_CONFIG))
        self.rpc_config.chmod(0o600)
        self.config_path = self.base / "bridge.json"
        self.state = self.base / "state"
        self.store_path = self.state / "transport.sqlite"
        self.config_value = {"schema": bridge.CONFIG_SCHEMA, "workshop_root": str(self.workshop),
                             "state_dir": str(self.state), "store_path": str(self.store_path),
                             "signal_cli": str(self.cli), "signal_data_dir": str(self.data),
                             "signal_rpc_config": str(self.rpc_config),
                             "account": ACCOUNT, "owner_id": OWNER}
        self.config_path.write_text(json.dumps(self.config_value))
        self.config_path.chmod(0o600)
        self.config = bridge.Config(self.config_path)
        self.clock = [0.0]

    def enable_agent_tools(self):
        assets = self.base / "assets"
        assets.mkdir(exist_ok=True)
        portrait = assets / "portrait.png"
        portrait.write_bytes(b"\x89PNG\r\n\x1a\n" + b"test-image")
        self.config_value["agent_tools"] = {"asset_roots": [str(assets)],
                                              "portrait_path": str(portrait)}
        self.config_path.write_text(json.dumps(self.config_value))
        self.config = bridge.Config(self.config_path)
        return assets, portrait

    def open_bridge(self):
        store = signal_store.Store(self.store_path, owner_id=OWNER)
        return store, bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                    clock=lambda: self.clock[0])

    def completed(self, burst, text="A bounded answer", *, status="completed"):
        run_id = "run-old"
        wake.claim(self.workshop, run_id)
        wake.delivered(self.workshop, run_id)
        run = self.workshop / "runs" / run_id
        run.mkdir(parents=True, exist_ok=True)
        started = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=1)
        receipt = {"schema": "mneme.workshop.receipt.v2", "run_id": run_id,
                   "status": status, "started_at": started.isoformat(),
                   "event_count": 1, "event_ids": [{"source": bridge.SOURCE, "event_id": burst["event_id"]}]}
        result = {"status": "progress", "summary": "Done", "artifacts": [],
                  "next_step": "", "memory_candidates": [], "next_wake_seconds": 3600,
                  "replies": ([{"source": bridge.SOURCE, "event_id": burst["event_id"], "text": text}]
                              if text else [])}
        (run / "receipt.json").write_text(json.dumps(receipt))
        (run / "result.json").write_text(json.dumps(result))
        (self.workshop / ".workshop.lock").touch()
        return run, receipt

    def test_explicit_init_status_and_config_refusal(self):
        self.assertFalse(self.state.exists())
        self.assertEqual(bridge.init(self.config)["status"], "initialized")
        self.assertEqual(bridge.status(self.config)["store"]["generation"], 0)
        with self.assertRaises(bridge.Refusal):
            bridge.init(self.config)
        wrong = dict(self.config_value, owner_id="not-a-uuid")
        self.config_path.write_text(json.dumps(wrong))
        with self.assertRaises(bridge.Refusal):
            bridge.Config(self.config_path)

    def test_init_legacy_queue_refuses_before_creating_signal_state(self):
        queue = self.workshop / "events.json"
        queue.write_text(json.dumps({"schema": wake.PREVIOUS_SCHEMA,
                                     "next_sequence": 1, "records": []}))
        before = queue.read_bytes()
        with self.assertRaisesRegex(bridge.Refusal, "Upgrade the existing workshop queue"):
            bridge.init(self.config)
        self.assertFalse(self.state.exists())
        self.assertEqual(queue.read_bytes(), before)

    def test_run_legacy_queue_refuses_before_reception_or_retry_state_mutation(self):
        bridge.init(self.config)
        queue = self.workshop / "events.json"
        queue.write_text(json.dumps({"schema": wake.PREVIOUS_SCHEMA,
                                     "next_sequence": 1, "records": []}))
        before_queue, before_ledger = queue.read_bytes(), self.store_path.read_bytes()
        with patch.object(signal_store.Store, "recover_uncertain_sends") as recover:
            with self.assertRaisesRegex(wake.Refusal, "Legacy event queue"):
                bridge.run(self.config, rpc_factory=lambda *_: self.fail("must not start Signal"))
        recover.assert_not_called()
        self.assertEqual(queue.read_bytes(), before_queue)
        self.assertEqual(self.store_path.read_bytes(), before_ledger)

    def test_upgrade_is_explicit_and_holds_bridge_and_workshop_leases(self):
        bridge.init(self.config)
        with self.assertRaisesRegex(bridge.Refusal, "Pause the workshop"):
            bridge.upgrade(self.config)
        self.assertEqual(wake.upgrade_version(self.workshop), wake.SCHEMA)
        (self.workshop / "PAUSED").touch()
        with bridge._lock(self.state):
            with self.assertRaisesRegex(bridge.Refusal, "already running"):
                bridge.upgrade(self.config)
        with wake._workshop_locked(self.workshop):
            with self.assertRaises(wake.Busy):
                bridge.upgrade(self.config)
        self.assertEqual(bridge.upgrade(self.config), {
            "status": "already_current", "ledger_schema": signal_store.SCHEMA, "queue_schema": wake.SCHEMA})
        self.assertEqual(bridge.upgrade(self.config)["status"], "already_current")

    def legacy_pair(self):
        bridge.init(self.config)
        # A named prior v2 layout, without synthesized seen data. Genuine prior
        # implementation compatibility is tested by the store/queue suites.
        with sqlite3.connect(self.store_path) as conn:
            conn.execute("ALTER TABLE messages DROP COLUMN seen")
            conn.execute("UPDATE metadata SET value=? WHERE key='schema'", (signal_store.V2_SCHEMA,))
            conn.execute("PRAGMA user_version=2")
        (self.workshop / "events.json").write_text(json.dumps({
            "schema": wake.PREVIOUS_SCHEMA, "next_sequence": 1, "records": []}))
        (self.workshop / "PAUSED").touch()

    def test_upgrade_interrupted_after_queue_publish_retries_named_partial_pair(self):
        self.legacy_pair()
        with patch.object(signal_store, "upgrade", side_effect=OSError("interrupted ledger upgrade")):
            with self.assertRaisesRegex(OSError, "interrupted"):
                bridge.upgrade(self.config)
        self.assertEqual(wake.upgrade_version(self.workshop), wake.SCHEMA)
        self.assertEqual(signal_store.upgrade_version(self.store_path, owner_id=OWNER), 2)
        self.assertEqual(bridge.upgrade(self.config)["status"], "upgraded")
        self.assertEqual(signal_store.upgrade_version(self.store_path, owner_id=OWNER), 3)
        self.assertEqual(bridge.upgrade(self.config)["status"], "already_current")

    def test_upgrade_unknown_either_store_is_refused_before_other_changes(self):
        self.legacy_pair()
        queue = self.workshop / "events.json"
        before_queue = queue.read_bytes()
        with sqlite3.connect(self.store_path) as conn:
            conn.execute("PRAGMA user_version=999")
        before_ledger = self.store_path.read_bytes()
        with self.assertRaises(signal_store.StoreError):
            bridge.upgrade(self.config)
        self.assertEqual(queue.read_bytes(), before_queue)
        self.assertEqual(self.store_path.read_bytes(), before_ledger)
        with sqlite3.connect(self.store_path) as conn:
            conn.execute("PRAGMA user_version=2")
        queue.write_text('{"schema":"unknown"}')
        before_ledger = self.store_path.read_bytes()
        with self.assertRaises(wake.Refusal):
            bridge.upgrade(self.config)
        self.assertEqual(self.store_path.read_bytes(), before_ledger)
        self.assertEqual(queue.read_text(), '{"schema":"unknown"}')

    def test_lifetime_lock_denies_second_runner_and_releases_after_error(self):
        bridge.init(self.config)
        with self.assertRaisesRegex(RuntimeError, "synthetic failure"):
            with bridge._lock(self.state):
                self.assertEqual(bridge.status(self.config), {"status": "running"})
                with self.assertRaisesRegex(bridge.Refusal, "already running"):
                    with bridge._lock(self.state):
                        pass
                raise RuntimeError("synthetic failure")
        self.assertEqual(bridge.status(self.config)["status"], "ready")
        with bridge._lock(self.state):
            pass  # the next process can reopen the released lease

    def test_optional_profile_config_admitted_before_init_or_refused_without_side_effect(self):
        self.assertIsNone(self.config.agent_tools)
        assets, portrait = self.enable_agent_tools()
        self.assertEqual(self.config.agent_tools["portrait_path"], portrait)
        self.assertEqual(self.config.agent_tools["asset_roots"], (assets,))
        invalid = dict(self.config_value)
        invalid["agent_tools"] = dict(invalid["agent_tools"], extra=True)
        self.config_path.write_text(json.dumps(invalid))
        with self.assertRaisesRegex(bridge.Refusal, "agent tools"):
            bridge.Config(self.config_path)
        self.assertFalse(self.state.exists())
        invalid["agent_tools"] = {"asset_roots": [str(assets)],
                                    "portrait_path": str(self.base / "outside.png")}
        self.config_path.write_text(json.dumps(invalid))
        with self.assertRaisesRegex(bridge.Refusal, "outside admitted roots"):
            bridge.Config(self.config_path)
        self.assertFalse(self.state.exists())
        portrait.write_text("not a PNG")
        self.config_path.write_text(json.dumps(self.config_value))
        with self.assertRaisesRegex(bridge.Refusal, "profile portrait"):
            bridge.Config(self.config_path)
        self.assertFalse(self.state.exists())

    def test_agent_server_polled_under_bridge_lease_before_inbox_tick(self):
        self.enable_agent_tools()
        bridge.init(self.config)
        order = []
        owner = self

        class StopLoop(Exception):
            pass

        class RunRpc:
            def __init__(self):
                self.polls = 0
            def __enter__(self):
                return self
            def __exit__(self, *_):
                order.append("rpc_exit")
            def start(self):
                return self
            def poll(self, timeout=0):
                self.polls += 1
                if self.polls > 1:
                    raise StopLoop()
                order.append("rpc_poll")

        class Agent:
            def __enter__(self):
                with owner.assertRaisesRegex(bridge.Refusal, "already running"):
                    with bridge._lock(owner.state):
                        pass
                order.append("agent_enter")
                return self
            def __exit__(self, *_):
                order.append("agent_exit")
            def poll(self, _rpc, *, paused=False, send_message=None, read_history=None):
                order.append(("agent_poll", paused, callable(send_message), callable(read_history)))
                return False

        for paused in (False, True):
            order.clear()
            marker = self.workshop / "PAUSED"
            if paused:
                marker.touch()
            else:
                marker.unlink(missing_ok=True)
            with patch.object(bridge.signal_tools, "SignalToolsServer", return_value=Agent()) as factory:
                with patch.object(bridge.Bridge, "tick", side_effect=lambda _rpc: order.append("tick")):
                    with self.assertRaises(StopLoop):
                        bridge.run(self.config, rpc_factory=lambda *_: RunRpc())
            factory.assert_called_once_with(self.state / "signal-tools.sock",
                                            asset_roots=self.config.agent_tools["asset_roots"],
                                            portrait_path=self.config.agent_tools["portrait_path"])
            self.assertEqual(order, ["agent_enter", "rpc_poll", ("agent_poll", paused, True, True),
                                     "tick", "agent_exit", "rpc_exit"])

    def test_spool_before_processing_owner_filter_and_exact_replay(self):
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                  clock=lambda: self.clock[0])
            actor.receive(notification("foreign", owner=ACCOUNT))
            actor.receive(notification("group", timestamp=1001, extras={"groupInfo": {"id": "g"}}))
            self.assertEqual(store.status()["generation"], 0)
            self.assertEqual(actor.ignored, 2)
            exceptional = notification("do not ingest", timestamp=1002)
            exceptional["params"]["result"]["exception"] = {"message": "not a chat"}
            actor.receive(exceptional)
            self.assertEqual(store.status()["generation"], 0)
            with patch.object(actor, "_consume", side_effect=signal_store.CapacityError("blocked")):
                with self.assertRaises(signal_store.CapacityError):
                    actor.receive(notification("hello"))
            self.assertEqual(len(actor.spool._files()[0]), 1)
            actor.spool.replay(actor._consume)
            self.assertEqual(store.status()["generation"], 1)
            actor.receive(notification("hello"))
            self.assertEqual(store.status()["generation"], 1)
            self.assertEqual(actor.spool._files()[0], [])

    def test_interrupted_spool_write_recovers_only_complete_frame(self):
        bridge.init(self.config)
        spool = bridge.Spool(self.state / "spool")
        final = "000000000001-" + "a" * 32 + ".json"
        temporary = spool.directory / ("." + final + ".tmp")
        temporary.write_bytes(json.dumps(notification()).encode()[:10])
        observed = bridge.status(self.config)
        self.assertEqual(observed["interrupted_spool_entries"], 1)
        self.assertTrue(temporary.exists())  # status is observational
        with self.assertRaisesRegex(bridge.Refusal, "incomplete receive spool"):
            spool.replay(lambda _raw: None)
        self.assertTrue(temporary.exists())  # not silently erased after upstream ACK
        temporary.write_bytes(json.dumps(notification()).encode() + b"\n")
        seen = []
        self.assertEqual(spool.replay(seen.append), 1)
        self.assertEqual(seen, [notification()])
        self.assertFalse(temporary.exists())
        self.assertFalse((spool.directory / final).exists())

    def test_spool_publication_and_removal_failure_cuts_reopen(self):
        bridge.init(self.config)
        spool = bridge.Spool(self.state / "spool")
        for cut in ("file_fsync", "rename", "directory_fsync"):
            with self.subTest(cut=cut):
                if cut == "file_fsync":
                    target, name = bridge.os, "fsync"
                elif cut == "rename":
                    target, name = bridge.os, "replace"
                else:
                    target, name = bridge, "_sync_dir"
                with patch.object(target, name, side_effect=OSError("injected publication cut")):
                    with self.assertRaisesRegex(OSError, "publication cut"):
                        spool.append(notification(timestamp=1000 + len(cut)))
                # An interrupted complete temp or already-renamed final is
                # recoverable; neither was offered to the consumer prematurely.
                reopened = bridge.Spool(self.state / "spool")
                seen = []
                self.assertEqual(reopened.replay(seen.append), 1)
                self.assertEqual(len(seen), 1)
                self.assertEqual(reopened.inspect(), (0, 0))
        spool.append(notification())
        with patch.object(bridge, "_sync_dir", side_effect=OSError("injected unlink cut")):
            with self.assertRaisesRegex(OSError, "unlink cut"):
                spool.replay(lambda _raw: None)
        self.assertEqual(bridge.Spool(self.state / "spool").inspect(), (0, 0))

    def test_quiet_burst_pause_and_idempotent_wake_publication(self):
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                  clock=lambda: self.clock[0])
            actor.receive(notification())
            self.clock[0] = 9
            actor.tick(FakeRpc())
            self.assertEqual(wake.active_records(self.workshop), [])
            (self.workshop / "PAUSED").touch()
            self.clock[0] = 10
            actor.tick(FakeRpc())
            self.assertEqual(wake.active_records(self.workshop), [])
            (self.workshop / "PAUSED").unlink()
            burst = store.prepare_burst(10)
            event = {"source": bridge.SOURCE, "event_id": burst["event_id"], "kind": bridge.KIND,
                     "summary": "Private owner conversation burst", "body": burst["body"], "reference": ""}
            wake.enqueue(self.workshop, event)  # crash after queue publication, before mark
            actor.tick(FakeRpc())
            self.assertEqual(len(wake.active_records(self.workshop)), 1)
            self.assertEqual(store.pending_bursts(), [])
            self.assertEqual(store.awaiting_replies()[0]["event_id"], burst["event_id"])

    def test_retryable_queue_backpressure_keeps_receiver_alive(self):
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                  clock=lambda: self.clock[0])
            actor.receive(notification())
            self.clock[0] = 10
            for reason in ("Event queue busy; retry after the current writer finishes",
                           "Event queue is full; inspect or archive it explicitly"):
                with patch.object(wake, "enqueue", side_effect=wake.Refusal(reason)):
                    actor.tick(FakeRpc())
                self.assertEqual(len(store.pending_bursts()), 1)
                self.assertEqual(store.awaiting_replies(), [])
            actor.tick(FakeRpc())
            self.assertEqual(len(store.awaiting_replies()), 1)

    def test_completed_proof_then_fixed_owner_send_and_no_reply_terminal(self):
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                  clock=lambda: self.clock[0])
            actor.receive(notification())
            self.clock[0] = 10
            actor.tick(FakeRpc())
            burst = store.awaiting_replies()[0]
            run, receipt = self.completed(burst, status="running")
            rpc = FakeRpc()
            actor.tick(rpc)
            self.assertEqual(rpc.sent, [])
            receipt["status"] = "completed"
            (run / "receipt.json").write_text(json.dumps(receipt))
            actor.tick(rpc)
            self.assertEqual(rpc.sent, [(OWNER, "A bounded answer")])
            self.assertEqual(store.status()["unanswered"], 0)

    def test_new_arrival_before_send_supersedes_old_answer(self):
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                  clock=lambda: self.clock[0])
            actor.receive(notification())
            self.clock[0] = 10
            actor.tick(FakeRpc())
            burst = store.awaiting_replies()[0]
            self.completed(burst)
            rpc = FakeRpc(on_poll=lambda: actor.receive(notification("new", timestamp=1001)))
            actor.tick(rpc)
            self.assertEqual(rpc.sent, [])
            self.assertEqual(store.status()["generation"], 2)
            self.assertEqual(store.status()["unanswered"], 2)

    def test_completed_empty_reply_is_terminal_without_send(self):
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                  clock=lambda: self.clock[0])
            actor.receive(notification())
            self.clock[0] = 10
            actor.tick(FakeRpc())
            self.completed(store.awaiting_replies()[0], text="")
            rpc = FakeRpc()
            actor.tick(rpc)
            self.assertEqual(rpc.sent, [])
            self.assertEqual(store.awaiting_replies(), [])
            self.assertEqual(store.pending_outbox(), [])

    def test_bounded_receive_poll_backlog_defers_send(self):
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                  clock=lambda: self.clock[0])
            actor.receive(notification())
            self.clock[0] = 10
            actor.tick(FakeRpc())
            self.completed(store.awaiting_replies()[0])
            backlog = FakeRpc(caught_up=False)
            actor.tick(backlog)
            self.assertEqual(backlog.sent, [])
            self.assertEqual(len(store.pending_outbox()), 1)
            drained = FakeRpc()
            actor.tick(drained)
            self.assertEqual(drained.sent, [(OWNER, "A bounded answer")])

    def test_mismatched_event_payload_cannot_attach_completed_reply(self):
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                  clock=lambda: self.clock[0])
            actor.receive(notification())
            self.clock[0] = 10
            actor.tick(FakeRpc())
            burst = store.awaiting_replies()[0]
            self.completed(burst)
            real_lookup = wake.lookup
            def wrong_body(root, source, event_id):
                found = real_lookup(root, source, event_id)
                found["record"]["event"]["body"] += " altered"
                return found
            with patch.object(wake, "lookup", side_effect=wrong_body):
                with self.assertRaisesRegex(bridge.Refusal, "payload mismatch"):
                    actor.tick(FakeRpc())

    def test_archived_completed_reply_survives_run_relocation(self):
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                  clock=lambda: self.clock[0])
            actor.receive(notification())
            self.clock[0] = 10
            actor.tick(FakeRpc())
            burst = store.awaiting_replies()[0]
            run, _receipt = self.completed(burst)
            (self.workshop / "PAUSED").touch()
            wake.archive(self.workshop)
            self.assertFalse(run.exists())
            (self.workshop / "PAUSED").unlink()
            rpc = FakeRpc()
            actor.tick(rpc)
            self.assertEqual(rpc.sent, [(OWNER, "A bounded answer")])

    def test_archived_outbox_not_blocked_by_unrelated_runner_lock(self):
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                  clock=lambda: self.clock[0])
            actor.receive(notification())
            self.clock[0] = 10
            actor.tick(FakeRpc())
            self.completed(store.awaiting_replies()[0])
            (self.workshop / "PAUSED").touch()
            wake.archive(self.workshop)
            (self.workshop / "PAUSED").unlink()
            lock = os.open(self.workshop / ".workshop.lock", os.O_RDONLY)
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                rpc = FakeRpc()
                actor.tick(rpc)
                self.assertEqual(rpc.sent, [(OWNER, "A bounded answer")])
            finally:
                os.close(lock)

    def test_agent_tools_proactive_send_durable_exact_retry_and_conflict(self):
        self.enable_agent_tools()
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"))
            rpc = FakeRpc()
            args = {"request_id": "proactive-1", "text": "Unprompted but authorized"}
            first = actor.send_message(rpc, args)
            self.assertEqual(first, {"outcome": "accepted", "timestamp_ms": 1234})
            self.assertEqual(actor.send_message(rpc, args), first)
            self.assertEqual(rpc.sent, [(OWNER, args["text"])])
            self.assertEqual(actor.send_message(rpc, dict(args, text="different")),
                             {"outcome": "failed", "reason": "request_id_conflict"})
            self.assertEqual(rpc.sent, [(OWNER, args["text"])])

    def test_agent_tools_bound_reply_and_inbound_only_wakes(self):
        self.enable_agent_tools()
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                  clock=lambda: self.clock[0])
            actor.receive(notification())
            self.clock[0] = 10
            actor.tick(FakeRpc())
            burst = store.awaiting_replies()[0]
            self.completed(burst)
            rpc = FakeRpc()
            actor.tick(rpc)
            self.assertEqual(rpc.sent, [])  # even a completed final result cannot send
            args = {"request_id": "bound-1", "text": "Explicit answer", "reply_to": burst["event_id"]}
            self.assertEqual(actor.send_message(rpc, args)["outcome"], "accepted")
            self.assertEqual(store.status()["unanswered"], 0)
            self.assertEqual(rpc.sent, [(OWNER, "Explicit answer")])

    def test_agent_tools_stale_reply_and_uncaught_receive_buffer_do_not_send(self):
        self.enable_agent_tools()
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                  clock=lambda: self.clock[0])
            actor.receive(notification())
            self.clock[0] = 10
            actor.tick(FakeRpc())
            burst = store.awaiting_replies()[0]
            stale = {"request_id": "stale-1", "text": "Old answer", "reply_to": burst["event_id"]}
            racing = FakeRpc(on_poll=lambda: actor.receive(notification("new", timestamp=1001)))
            self.assertEqual(actor.send_message(racing, stale)["outcome"], "superseded")
            self.assertEqual(racing.sent, [])
            held = FakeRpc(caught_up=False)
            proactive = {"request_id": "held-1", "text": "Wait for input"}
            self.assertEqual(actor.send_message(held, proactive)["outcome"], "pending")
            self.assertEqual(held.sent, [])
            sent = FakeRpc()
            self.assertEqual(actor.send_message(sent, proactive)["outcome"], "accepted")
            self.assertEqual(sent.sent, [(OWNER, "Wait for input")])

    def test_agent_tools_unknown_after_transport_loss_survives_restart_without_replay(self):
        self.enable_agent_tools()
        bridge.init(self.config)
        args = {"request_id": "uncertain-1", "text": "Did it arrive?"}
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"))
            rpc = FakeRpc(outcome="transport_error")
            self.assertEqual(actor.send_message(rpc, args)["outcome"], "unknown")
            self.assertEqual(rpc.sent, [(OWNER, args["text"])])
        with self.open_bridge()[0] as store:
            store.recover_uncertain_sends()
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"))
            rpc = FakeRpc()
            self.assertEqual(actor.send_message(rpc, args)["outcome"], "unknown")
            self.assertEqual(rpc.sent, [])

    def test_interrupted_direct_send_intent_is_not_replayed(self):
        self.enable_agent_tools()
        bridge.init(self.config)
        args = {"request_id": "interrupted-1", "text": "Maybe sent"}
        with self.open_bridge()[0] as store:
            store.queue_direct(args["request_id"], args["text"])
            self.assertTrue(store.begin_direct_send(args["request_id"]))
        with self.open_bridge()[0] as store:
            self.assertGreaterEqual(store.recover_uncertain_sends(), 1)
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"))
            rpc = FakeRpc()
            self.assertEqual(actor.send_message(rpc, args)["outcome"], "unknown")
            self.assertEqual(rpc.sent, [])

    def test_live_history_consumes_wake_without_workshop_lock_or_answering(self):
        self.enable_agent_tools()
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                  clock=lambda: self.clock[0])
            actor.receive(notification("Question"))
            self.clock[0] = 10
            actor.tick(FakeRpc())
            burst = store.awaiting_replies()[0]
            with wake._workshop_locked(self.workshop):
                result = actor.read_history({"limit": 12})
            self.assertEqual([item["text"] for item in result["entries"]], ["Question"])
            self.assertEqual(store.status()["unanswered"], 1)
            self.assertEqual(store.status()["seen"], 1)
            self.assertEqual(wake.active_records(self.workshop), [])
            proof = wake.lookup(self.workshop, bridge.SOURCE, burst["event_id"])
            self.assertEqual(proof["terminal"]["status"], "consumed")
            self.assertIsNone(bridge._reply_proof(self.workshop, burst, active_allowed=True))
            self.assertEqual(actor.read_history({"limit": 12}), result)
            actor.tick(FakeRpc())
            self.assertEqual(wake.active_records(self.workshop), [])
            self.assertEqual(store.status()["unanswered"], 1)

    def check_seen_backlog_not_published(self, *, prebuilt):
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                  clock=lambda: self.clock[0])
            actor.receive(notification("Read live"))
            if prebuilt:
                store.prepare_burst(10)
            actor.read_history({"limit": 12})
            self.clock[0] = 10
            actor.tick(FakeRpc())
            self.assertEqual(wake.active_records(self.workshop), [])
            self.assertEqual(store.status()["unanswered"], 1)
            actor.receive(notification("New unseen", timestamp=1001))
            self.clock[0] = 20
            actor.tick(FakeRpc())
            events = wake.active_records(self.workshop)
            self.assertEqual(len(events), 1)
            self.assertIn("New unseen", events[0]["event"]["body"])
            self.assertNotIn("Read live", events[0]["event"]["body"])

    def test_read_before_quiet_never_publishes_seen_backlog(self):
        self.check_seen_backlog_not_published(prebuilt=False)

    def test_prebuilt_burst_never_publishes_seen_backlog(self):
        self.check_seen_backlog_not_published(prebuilt=True)

    def test_bounded_history_keeps_omitted_overlap_and_outgoing_competition_pending(self):
        self.enable_agent_tools()
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                  clock=lambda: self.clock[0])
            actor.receive(notification("A", timestamp=1000))
            self.clock[0] = 10
            actor.tick(FakeRpc())
            actor.receive(notification("B", timestamp=1100))
            self.clock[0] = 20
            actor.tick(FakeRpc())
            self.assertEqual(len(wake.active_records(self.workshop)), 2)  # {A}, {A,B}
            actor.send_message(FakeRpc(), {"request_id": "proactive", "text": "Outgoing"})
            result = actor.read_history({"limit": 1})
            self.assertEqual(result["entries"][0]["direction"], "outgoing")
            self.assertEqual(store.status()["seen"], 0)
            result = actor.read_history({"limit": 2})
            self.assertEqual([item["text"] for item in result["entries"]], ["B", "Outgoing"])
            self.assertEqual(store.status()["seen"], 1)
            self.assertEqual(len(wake.active_records(self.workshop)), 2)
            actor.read_history({"limit": 3})
            self.assertEqual(wake.active_records(self.workshop), [])
            self.assertEqual(store.status()["unanswered"], 2)

    def test_history_timestamp_order_does_not_ack_later_generation_or_oversized_item(self):
        self.enable_agent_tools()
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                  clock=lambda: self.clock[0])
            actor.receive(notification("Earlier generation, later clock", timestamp=2000))
            actor.receive(notification("Later generation, earlier clock", timestamp=1000))
            self.clock[0] = 10
            actor.tick(FakeRpc())
            actor.read_history({"limit": 1})
            self.assertEqual(store.status()["seen"], 1)
            self.assertEqual(len(wake.active_records(self.workshop)), 1)
            actor.receive(notification("🪨" * 1100, timestamp=3000))
            result = actor.read_history({"limit": 20})
            self.assertEqual(result["entries"], [])
            self.assertTrue(result["truncated"])
            self.assertEqual(store.status()["seen"], 1)
            self.assertEqual(store.status()["unanswered"], 3)

    def test_seen_commit_survives_queue_busy_then_restart_and_reconciles_while_paused(self):
        self.enable_agent_tools()
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                  clock=lambda: self.clock[0])
            actor.receive(notification())
            self.clock[0] = 10
            actor.tick(FakeRpc())
            with wake._locked(self.workshop):
                observed = actor.read_history({"limit": 12})
                actor.tick(FakeRpc())  # no republishing while reconciliation is busy
            self.assertEqual(observed["entries"][0]["text"], "hello")
            self.assertEqual(store.status()["seen"], 1)
            self.assertEqual(len(wake.active_records(self.workshop)), 1)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"))
            (self.workshop / "PAUSED").touch()
            actor.tick(FakeRpc())
            self.assertEqual(wake.active_records(self.workshop), [])
            self.assertEqual(store.status()["unanswered"], 1)

    def test_history_commit_precedes_queue_write_and_lost_ack_retries(self):
        self.enable_agent_tools()
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                  clock=lambda: self.clock[0])
            actor.receive(notification())
            self.clock[0] = 10
            actor.tick(FakeRpc())
            def crash_after_seen(*_):
                self.assertFalse(store.conn.in_transaction)
                self.assertEqual(store.status()["seen"], 1)
                raise OSError("queue publication interrupted")
            with patch.object(wake, "consume_signal_events", side_effect=crash_after_seen):
                with self.assertRaisesRegex(OSError, "interrupted"):
                    actor.read_history({"limit": 12})
            self.assertEqual(len(wake.active_records(self.workshop)), 1)
            actor.tick(FakeRpc())
            self.assertEqual(wake.active_records(self.workshop), [])

    def test_projection_validation_precedes_seen_and_run_owned_events_are_not_consumed(self):
        self.enable_agent_tools()
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"),
                                  clock=lambda: self.clock[0])
            actor.receive(notification())
            self.clock[0] = 10
            actor.tick(FakeRpc())
            with patch.object(store, "read_history", return_value={"entries": [{"text": "x" * 4100}],
                                                                  "uncertain": [], "truncated": True}):
                with self.assertRaises(bridge.signal_tools.SignalToolsError):
                    actor.read_history({"limit": 12})
            self.assertEqual(store.status()["seen"], 0)
            wake.claim(self.workshop, "existing-run")
            held = wake.active_records(self.workshop)
            actor.read_history({"limit": 12})
            self.assertEqual(wake.active_records(self.workshop), held)
            wake.delivered(self.workshop, "existing-run")
            delivered = wake.active_records(self.workshop)
            actor.read_history({"limit": 12})
            self.assertEqual(wake.active_records(self.workshop), delivered)
            self.assertEqual(store.status()["unanswered"], 1)

    def test_history_callback_reads_retained_conversation_while_paused_without_rpc(self):
        self.enable_agent_tools()
        bridge.init(self.config)
        with self.open_bridge()[0] as store:
            actor = bridge.Bridge(self.config, store, bridge.Spool(self.state / "spool"))
            actor.receive(notification("Question", timestamp=1000))
            rpc = FakeRpc()
            actor.send_message(rpc, {"request_id": "history-send-1", "text": "Answer"})
            polls, sends = rpc.polls, list(rpc.sent)
            (self.workshop / "PAUSED").touch()
            observed = actor.read_history({"limit": 12})
            self.assertEqual([(item["direction"], item["text"]) for item in observed["entries"]],
                             [("incoming", "Question"), ("outgoing", "Answer")])
            self.assertEqual(observed["uncertain"], [])
            self.assertFalse(observed["truncated"])
            self.assertEqual((rpc.polls, rpc.sent), (polls, sends))


if __name__ == "__main__":
    unittest.main()

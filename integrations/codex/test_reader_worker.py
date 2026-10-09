"""Local-state and fake-runtime tests; no Codex, Mneme service or provider."""
from __future__ import annotations

import json
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import reader_worker as reader
from reader_test_support import PublicationHandoff

CARD_A = {"id": "01K123456789ABCDEFGHJKMNPQ", "summary": "A surprising link",
          "status": "active", "source": "test", "fingerprint": "a"}
CARD_B = {"id": "01K123456789ABCDEFGHJKMNPR", "summary": "A contradiction",
          "status": "active", "source": "test", "fingerprint": "b"}
PROJECT_DB = "01ARZ3NDEKTSV4RRFFQ69G5FAV"


class FakeRuntime:
    instances = []
    result = {"selected_ids": [CARD_A["id"]], "reason": "selected",
              "usage": {"input_tokens": 10, "output_tokens": 2},
              "elapsed_ms": 4, "provider_attempt": True}

    def __init__(self, config, scratch_dir):
        self.calls = []
        self.closed = False
        self.instances.append(self)

    def select(self, dialogue, cards):
        self.calls.append((dialogue, cards))
        return dict(self.result)

    def close(self):
        self.closed = True


class ReaderStateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.config = {"state_dir": self.root / "state", "service_config": self.root / "service.json",
                       "project_root": self.root, "reader_model": "gpt-6.1-sol", "librarian_effort": "medium"}
        self.config["service_config"].write_text("{}")
        FakeRuntime.instances.clear()
        FakeRuntime.result = {"selected_ids": [CARD_A["id"]], "reason": "selected",
                              "usage": {"input_tokens": 10, "output_tokens": 2},
                              "elapsed_ms": 4, "provider_attempt": True}

    def test_stewardship_preserves_selector_room_and_pending_priority(self):
        reader.notice(self.config,self.event(),True)
        self.assertFalse(reader._stewardship_reserve(self.config,"s1","stewardship:first"))
        reader._state(self.config,"s1",lambda data:(data.update(pending=None,inflight=None),True),create=False)
        self.assertTrue(reader._stewardship_reserve(self.config,"s1","stewardship:second"))
        self.assertTrue(reader._recording_account(self.config,"s1","stewardship:second",
            {"provider_attempt":True,"usage":{"input_tokens":50,"output_tokens":5}}))
        budget=reader.resolve(self.config)
        reader._state(self.config,"s1",lambda data:(data.update(attempts=budget.attempts-1),True),create=False)
        self.assertFalse(reader._stewardship_reserve(self.config,"s1","stewardship:third"))

    def test_stewardship_snapshot_is_fenced_by_reset_and_trivial_prompt(self):
        reader.notice(self.config,self.event(),True)
        reader.close_turn(self.config,"s1","t1")
        expected=reader._stewardship_snapshot(self.config,"s1")
        self.assertIsNotNone(expected)
        self.assertTrue(reader._stewardship_idle(self.config,"s1",expected))
        reader.reset(self.config,"s1")
        self.assertFalse(reader._stewardship_idle(self.config,"s1",expected))
        self.assertFalse(reader._stewardship_reserve(self.config,"s1","old",expected))
        expected=reader._stewardship_snapshot(self.config,"s1")
        reader.notice(self.config,self.event(turn="t2",prompt="ok"),False)
        self.assertFalse(reader._stewardship_idle(self.config,"s1",expected))
        self.assertFalse(reader._stewardship_reserve(self.config,"s1","old",expected))

    def test_recording_off_dominates_retained_stewardship_flag(self):
        self.config.update(recording_mode="off",memory_mode="async",tag_stewardship=True)
        reader.notice(self.config,self.event(),True)
        reader.close_turn(self.config,"s1","t1")
        with patch("stewardship.step") as step:
            reader.serve(self.config,"s1",runtime_factory=FakeRuntime,idle_seconds=.01)
        step.assert_not_called()

    def event(self, turn="t1", prompt="Design a useful thing"):
        return {"session_id": "s1", "turn_id": turn, "prompt": prompt}

    def test_touchstone_successor_refuses_stale_state_without_resetting_usage(self):
        reader.notice(self.config, self.event(), True)
        path, _ = reader._paths(self.config, "s1")
        stale = self.state()
        stale.update(schema="mneme.codex-reader.state.v1", attempts=3, input_tokens=11,
                     output_tokens=2, reservation={"paid": "old identity"})
        path.write_text(json.dumps(stale))
        before = path.read_bytes()
        ok, _ = reader._state(self.config, "s1", lambda data: (data.update(attempts=0), True))
        self.assertFalse(ok)
        self.assertEqual(path.read_bytes(), before)

    def test_disappeared_reference_redelivers_owner_without_rewriting_content(self):
        from test_touchstone_librarian import card, DB
        original = card()
        original["touchstone"]["references"][0]["resolution"] = "matches_snapshot"
        missing = json.loads(json.dumps(original))
        missing["touchstone"]["references"][0]["resolution"] = "missing"
        for turn, candidate, outcome in (("t1", original, "emitted"),
                                         ("t2", original, "duplicate"),
                                         ("t3", missing, "emitted"),
                                         ("t4", missing, "duplicate")):
            with self.subTest(turn=turn):
                event = self.event(turn)
                reader.notice(self.config, event, True)
                self.set_state(lambda data: data.update(ready={**data["pending"],
                    "cards": [candidate], "db_id": DB, "fence": data["fence"]}))
                result = reader.consume(self.config, event)
                self.assertEqual(result["outcome"], outcome)
                if outcome == "emitted":
                    self.assertEqual(result["cards"][0]["fingerprint"], original["fingerprint"])
                    self.assertEqual(result["displayed"][0]["displayed_view"]["touchstone"], candidate["touchstone"])
        self.assertEqual(len(self.state()["emitted"]), 2)

    def state(self):
        path, _ = reader._paths(self.config, "s1")
        return json.loads(path.read_text())

    def set_state(self, mutate):
        ok, _ = reader._state(self.config, "s1", lambda data: (mutate(data), True))
        self.assertTrue(ok)

    def test_v7_rebound_discards_optional_views_but_preserves_paid_identity_and_usage(self):
        hook = self.root / "hook.json"
        hook.write_text(json.dumps({"schema": "mneme.codex-hooks.config.v6"}))
        old = {**self.config, "_config_path": hook}
        reader.notice(old, self.event(), True)
        pending = self.state()["pending"]
        ok, _ = reader._state(old, "s1", lambda data: (data.update(
            reservation=pending, inflight=pending, ready={**pending, "cards": [CARD_A], "fence": data["fence"]},
            prior="old task context", emitted=[[CARD_A["id"], CARD_A["fingerprint"]]],
            attempts=3, input_tokens=11, output_tokens=2, cached_input_tokens=5,
            reasoning_output_tokens=1), True))
        self.assertTrue(ok)
        before = self.state()
        hook.write_text(json.dumps({"schema": "mneme.codex-hooks.config.v8"}))
        current = {**old}
        current.pop("_reader_config_pin", None)
        ok, _ = reader._state(current, "s1", lambda _data: (None, False))
        self.assertTrue(ok)
        after = self.state()
        for key in ("schema", "reservation", "inflight", "attempts", "input_tokens", "output_tokens",
                    "cached_input_tokens", "reasoning_output_tokens", "unknown_usage"):
            self.assertEqual(after[key], before[key], key)
        self.assertIsNone(after["ready"])
        self.assertIsNone(after["active"])
        self.assertIsNone(after["pending"])
        self.assertEqual((after["prior"], after["emitted"]), ("", []))
        self.assertNotEqual(after["generation"], before["generation"])
        self.assertGreater(after["context_generation"], before["context_generation"])
        # The old object's captured pin cannot publish or reserve fresh work.
        self.assertFalse(reader._state(old, "s1", lambda data: (data.update(ready=before["ready"]), True))[0])
        self.assertFalse(reader._recording_reserve(old, "s1", "obsolete-fresh-call"))
        self.assertEqual(self.state(), after)

    def test_stale_config_paid_result_settles_then_new_policy_resumes(self):
        for known in (True,False):
            with self.subTest(known=known):
                # Real shared digest includes exact hook bytes; config objects capture once.
                hook = self.root / "hook.json"
                hook.write_text(json.dumps({"schema": "mneme.codex-hooks.config.v6"}))
                config_a = {**self.config,"_config_path":hook, "schema": "mneme.codex-hooks.config.v6"}
                config_a.pop("_reader_config_pin",None)
                reader.notice(config_a,self.event("a"),True)
                holder = {}
                owner = self
                class OldRuntime(FakeRuntime):
                    def select(self,dialogue,cards):
                        hook.write_text(json.dumps({"schema": "mneme.codex-hooks.config.v8"}))
                        config_b = {**config_a,"librarian_effort":"high", "schema": "mneme.codex-hooks.config.v8"}
                        config_b.pop("_reader_config_pin",None)
                        holder["b"] = config_b
                        owner.assertEqual(reader.notice(config_b,owner.event("b"),True)["outcome"],"queued")
                        return {**self.result,"usage":{"input_tokens":10,"output_tokens":2} if known else None}
                with patch("hook_recall.collect_reader",return_value={"outcome":"ok","cards":[CARD_A]}), \
                     patch("hook_recall.register_reader_concerns") as writes:
                    reader.serve(config_a,"s1",runtime_factory=OldRuntime,idle_seconds=.05)
                writes.assert_not_called()
                config_b = holder["b"]
                state = owner.state()
                owner.assertIsNone(state["reservation"])
                owner.assertIsNone(state["inflight"])
                owner.assertIsNone(state["ready"])
                owner.assertEqual(state["active"]["turn"],"b")
                owner.assertEqual(state["pending"]["turn"],"b")
                owner.assertFalse(reader._recording_reserve(config_a,"s1","old-new-call"))
                with patch("hook_recall.collect_reader",return_value={"outcome":"ok","cards":[CARD_A]}) as native:
                    reader.serve(config_b,"s1",runtime_factory=FakeRuntime,idle_seconds=.05)
                state = owner.state()
                if known:
                    owner.assertTrue(native.called)
                    owner.assertFalse(state["unknown_usage"])
                    owner.assertEqual((state["attempts"],state["input_tokens"],state["output_tokens"]),(2,20,4))
                else:
                    native.assert_not_called()
                    owner.assertTrue(state["unknown_usage"])
                    owner.assertEqual(state["attempts"],1)
                # Independent session ledger for the second subcase.
                for path in reader._directory(self.config).glob("*"):
                    if path.is_file(): path.unlink()

    def test_notice_replay_and_trivial_invalidation(self):
        event = self.event()
        self.assertEqual(reader.notice(self.config, event, True)["outcome"], "queued")
        self.assertEqual(reader.notice(self.config, event, True)["outcome"], "replay")
        self.set_state(lambda data: data.update(ready={**data["pending"], "cards": [CARD_A], "fence": data["fence"]}))
        self.assertEqual(reader.notice(self.config, self.event("t2", "ok"), False)["outcome"], "skipped")
        self.assertIsNone(self.state()["ready"])
        self.assertEqual(reader.consume(self.config, {"session_id": "s1", "turn_id": "t1"})["cards"], [])
        self.assertLessEqual(len(self.state()["active"]["cue"].encode()),
                             reader.MAX_PROMPT + reader.MAX_FRAGMENT + 40)

    def test_once_only_emission_and_epoch(self):
        event = self.event()
        reader.notice(self.config, event, True)
        self.set_state(lambda data: data.update(ready={**data["pending"], "cards": [CARD_A, CARD_B], "fence": data["fence"]}))
        shown = reader.consume(self.config, event)
        self.assertEqual(shown["outcome"], "emitted")
        self.assertEqual([c["id"] for c in shown["cards"]], [CARD_A["id"], CARD_B["id"]])
        self.assertEqual(reader.consume(self.config, event)["outcome"], "empty")
        reader.notice(self.config, self.event("t2", "Another task"), True)
        self.set_state(lambda data: data.update(ready={**data["pending"], "cards": [CARD_A], "fence": data["fence"]}))
        self.assertEqual(reader.consume(self.config, self.event("t2"))["outcome"], "duplicate")
        reader.reset(self.config, "s1")
        self.assertEqual(self.state()["emitted"], [])
        self.assertIsNone(self.state()["ready"])

    def test_stop_reset_end_scrub_without_resetting_budget(self):
        event = self.event()
        reader.notice(self.config, event, True)
        self.set_state(lambda data: data.update(attempts=47, input_tokens=249999, output_tokens=19999))
        reader.close_turn(self.config, "s1", "t1")
        self.assertIsNone(self.state()["pending"])
        self.assertTrue(self.state()["active"]["closed"])
        reader.reset(self.config, "s1")
        self.assertEqual(self.state()["attempts"], 47)
        self.assertIsNone(self.state()["active"])
        reader.notice(self.config, self.event("t2"), True)
        reader.end_session(self.config, "s1")
        state = self.state()
        self.assertTrue(state["ended"])
        self.assertIsNone(state["active"])
        self.assertEqual(state["prior"], "")

    def test_busy_state_skips_instead_of_waiting(self):
        reader.notice(self.config, self.event(), True)
        _, lock = reader._paths(self.config, "s1")
        import fcntl
        with lock.open("a+b") as stream:
            fcntl.flock(stream, fcntl.LOCK_EX)
            results = []
            thread = threading.Thread(target=lambda: results.append(reader.consume(self.config, self.event())))
            start = time.monotonic()
            thread.start(); thread.join(1)
            self.assertFalse(thread.is_alive())
            self.assertLess(time.monotonic() - start, 1)
            self.assertEqual(results[0]["outcome"], "busy")

    def test_busy_reset_and_trivial_notice_fence_old_ready(self):
        import fcntl
        event = self.event()
        reader.notice(self.config, event, True)
        _, lock = reader._paths(self.config, "s1")
        for index, invalidate in enumerate((lambda: reader.reset(self.config, "s1"),
                                            lambda: reader.notice(self.config, self.event("t2", "ok"), False))):
            if index:
                reader.notice(self.config, event, True)
            previous_context = reader._context_token(self.config, "s1")
            self.set_state(lambda data: data.update(ready={**data["pending"],
                                                        "cards": [CARD_A], "fence": data["fence"]}))
            with lock.open("a+b") as stream:
                fcntl.flock(stream, fcntl.LOCK_EX)
                thread = threading.Thread(target=invalidate)
                thread.start(); thread.join(1)
                self.assertFalse(thread.is_alive())
            self.assertEqual(reader.consume(self.config, event)["cards"], [])
            if index == 0:
                self.assertNotEqual(reader._context_token(self.config, "s1"), previous_context)

    def test_late_stop_does_not_poison_newer_turn_when_state_free(self):
        reader.notice(self.config, self.event("t1"), True)
        reader.notice(self.config, self.event("t2", "new task"), True)
        self.set_state(lambda data: data.update(ready={**data["pending"],
                                                    "cards": [CARD_A], "fence": data["fence"]}))
        reader.close_turn(self.config, "s1", "t1")
        self.assertEqual(reader.consume(self.config, {"session_id": "s1", "turn_id": "t2"})["cards"], [CARD_A])
        self.assertEqual(self.state()["active"]["turn"], "t2")
        self.assertFalse(self.state()["active"]["closed"])

    def test_busy_late_stop_conservatively_drops_newer_ready(self):
        import fcntl
        reader.notice(self.config, self.event("t1"), True)
        reader.notice(self.config, self.event("t2", "new task"), True)
        self.set_state(lambda data: data.update(ready={**data["pending"],
                                                    "cards": [CARD_A], "fence": data["fence"]}))
        _, lock = reader._paths(self.config, "s1")
        with lock.open("a+b") as stream:
            fcntl.flock(stream, fcntl.LOCK_EX)
            thread = threading.Thread(target=lambda: reader.close_turn(self.config, "s1", "t1"))
            thread.start(); thread.join(1)
            self.assertFalse(thread.is_alive())
        self.assertEqual(reader.consume(self.config, {"session_id": "s1", "turn_id": "t2"})["cards"], [])

    def test_busy_matching_stop_suppresses_old_ready(self):
        import fcntl
        event = self.event("t1")
        reader.notice(self.config, event, True)
        self.set_state(lambda data: data.update(ready={**data["pending"],
                                                    "cards": [CARD_A], "fence": data["fence"]}))
        _, lock = reader._paths(self.config, "s1")
        with lock.open("a+b") as stream:
            fcntl.flock(stream, fcntl.LOCK_EX)
            thread = threading.Thread(target=lambda: reader.close_turn(self.config, "s1", "t1"))
            thread.start(); thread.join(1)
            self.assertFalse(thread.is_alive())
        self.assertEqual(reader.consume(self.config, event)["cards"], [])

    def test_busy_matching_then_busy_stale_stop_cannot_restore_ready(self):
        import fcntl
        reader.notice(self.config, self.event("t1"), True)
        reader.notice(self.config, self.event("t2", "new task"), True)
        self.set_state(lambda data: data.update(ready={**data["pending"],
                                                    "cards": [CARD_A], "fence": data["fence"]}))
        _, lock = reader._paths(self.config, "s1")
        with lock.open("a+b") as stream:
            fcntl.flock(stream, fcntl.LOCK_EX)
            for turn in ("t2", "t1"):
                thread = threading.Thread(target=lambda value=turn: reader.close_turn(self.config, "s1", value))
                thread.start(); thread.join(1)
                self.assertFalse(thread.is_alive())
        self.assertEqual(reader.consume(self.config, {"session_id": "s1", "turn_id": "t2"})["cards"], [])

    def test_worker_selects_only_original_cards_and_accounts_usage(self):
        reader.notice(self.config, self.event(), True)
        native = {"outcome": "ok", "cards": [CARD_A, CARD_B], "observation": {"schema": 1},
                  "db_id": PROJECT_DB}
        with patch("hook_recall.collect_reader", return_value=native), PublicationHandoff(reader, "s1") as handoff:
            worker = threading.Thread(target=lambda: reader.serve(self.config, "s1", runtime_factory=FakeRuntime, idle_seconds=0.5))
            worker.start()
            try:
                handoff.wait()
                consumed = reader.consume(self.config, self.event())
                self.assertEqual(consumed["cards"], [CARD_A])
                self.assertEqual(consumed["db_id"], PROJECT_DB)
                self.assertNotIn("selection", consumed)
                self.assertNotIn("last_selection", consumed)
                self.assertEqual(self.state()["last_selection"]["selected_ids"], [CARD_A["id"]])
            finally:
                reader.end_session(self.config, "s1")
                handoff.finish()
                worker.join(2)
            self.assertFalse(worker.is_alive())
        self.assertEqual(self.state()["attempts"], 1)
        self.assertEqual(self.state()["input_tokens"], 10)
        self.assertEqual(self.state()["output_tokens"], 2)
        self.assertIsNone(self.state()["cached_input_tokens"])
        self.assertIsNone(self.state()["reasoning_output_tokens"])
        self.assertTrue(FakeRuntime.instances[0].closed)
        self.assertNotIn("db_id", FakeRuntime.instances[0].calls[0][1][0])

    def test_ready_capacity_drops_learning_before_recall_and_recall_before_accounting(self):
        from test_routing_memory import binding
        reader.notice(self.config, self.event(), True)
        base = self.state()
        ready = {**base['pending'], 'cards': [{**CARD_A, 'routing_binding': binding()}],
                 'db_id': PROJECT_DB, 'fence': base['fence']}
        trial = {**base, 'ready': ready}
        size = len(json.dumps(trial, ensure_ascii=False, separators=(',', ':')).encode())
        base_size = len(json.dumps(base, ensure_ascii=False, separators=(',', ':')).encode())
        for allowance, keep_card in ((size - 100, True), (base_size + 50, False)):
            self.set_state(lambda data: data.update(ready=None))
            with self.subTest(keep_card=keep_card), patch.object(reader, 'MAX_STATE', allowance):
                self.set_state(lambda data: data.update(ready={**ready, 'cards': list(ready['cards'])},
                                                        input_tokens=13, output_tokens=3))
                state = self.state()
                self.assertEqual((state['input_tokens'], state['output_tokens']), (13, 3))
                if keep_card:
                    self.assertEqual(state['ready']['cards'], [CARD_A])
                    self.assertEqual(state['ready']['db_id'], PROJECT_DB)
                else:
                    self.assertIsNone(state['ready'])
                    self.assertEqual(state['counts']['dropped'], 1)

    def test_unknown_usage_soft_stops_future_work(self):
        reader.notice(self.config, self.event(), True)
        FakeRuntime.result = {"selected_ids": [CARD_A["id"]], "reason": "selected",
                              "usage": None, "provider_attempt": True, "elapsed_ms": 1}
        with patch("hook_recall.collect_reader", return_value={"outcome": "ok", "cards": [CARD_A]}):
            reader.serve(self.config, "s1", runtime_factory=FakeRuntime, idle_seconds=0.15)
        self.assertEqual(reader.consume(self.config, self.event())["cards"], [])
        self.assertTrue(self.state()["unknown_usage"])
        self.assertEqual(reader.notice(self.config, self.event("t2"), True)["outcome"], "budget")
        reader.reset(self.config, "s1")
        self.assertEqual(reader.notice(self.config, self.event("t3"), True)["outcome"], "budget")

    def test_stale_completion_discarded(self):
        from hook_recall import _unknown_discovery
        discovery = _unknown_discovery(adapter={"observed_unique_window": 1, "readbacks_attempted": 2, "work_budget_unread": 0, "graph_readback_omitted": 0, "graph_byte_omitted": 0,
                                               "readback_skipped": 0, "byte_budget_omitted": 0, "returned": 1})
        reader.notice(self.config, self.event(), True)
        class SupersedingRuntime(FakeRuntime):
            def select(self, dialogue, cards):
                reader.notice(self.config, {"session_id": "s1", "turn_id": "t2", "prompt": "new task"}, True)
                return dict(FakeRuntime.result)
            def __init__(self, config, scratch_dir):
                super().__init__(config, scratch_dir)
                self.config = config
        with patch("hook_recall.collect_reader", return_value={"outcome": "ok", "cards": [CARD_A],
                                                              "discovery": discovery}):
            reader.serve(self.config, "s1", runtime_factory=SupersedingRuntime, idle_seconds=0.15)
        self.assertEqual(reader.consume(self.config, self.event())["cards"], [])
        self.assertEqual(self.state()["attempts"], 2)  # newest pending job also ran
        self.assertEqual((self.state()["input_tokens"], self.state()["output_tokens"]), (20, 4))
        self.assertEqual(self.state()["last_selection"]["discovery"], discovery)

    def test_restart_after_reserved_call_halts_admission(self):
        reader.notice(self.config, self.event(), True)
        def crashed(data):
            data["inflight"] = data["pending"]
            data["reservation"] = data["pending"]
            data["attempts"] = 1
        self.set_state(crashed)
        reader.notice(self.config, self.event("t2", "new work"), True)
        with patch("hook_recall.collect_reader") as native:
            reader.serve(self.config, "s1", runtime_factory=FakeRuntime, idle_seconds=0.05)
        native.assert_not_called()
        self.assertTrue(self.state()["unknown_usage"])
        self.assertEqual(self.state()["attempts"], 1)
        self.assertEqual(reader.notice(self.config, self.event("t3"), True)["outcome"], "budget")

    def test_more_than_eight_candidates_reach_selector(self):
        cards = [{**CARD_A, "id":"01ARZ3NDEKTSV4RRFFQ69G5FA" + str(i)} for i in range(10)]
        runtime = FakeRuntime(self.config, self.root)
        runtime.result = {**FakeRuntime.result, "selected_ids":[cards[-1]["id"]]}
        with patch("hook_recall.collect_reader", return_value={"outcome":"ok", "cards":cards}):
            result = reader._execute(self.config, "Current task: ordinary question", runtime,
                                     lambda: True, set())
        self.assertEqual(len(runtime.calls[0][1]), 10)
        self.assertEqual(result["cards"], [cards[-1]])

    def test_select_publish_consume_more_than_two_cards_in_source_order(self):
        cards = [{**CARD_A, "id": "01ARZ3NDEKTSV4RRFFQ69G5FA" + str(i),
                  "fingerprint": str(i) * 64} for i in range(8)]
        FakeRuntime.result = {**FakeRuntime.result, "selected_ids": [c["id"] for c in reversed(cards)]}
        event = self.event()
        reader.notice(self.config, event, True)
        with patch("hook_recall.collect_reader", return_value={"outcome": "ok", "cards": cards,
                                                              "db_id": PROJECT_DB}), \
             PublicationHandoff(reader, "s1") as handoff:
            worker = threading.Thread(target=lambda: reader.serve(self.config, "s1", runtime_factory=FakeRuntime,
                                                                   idle_seconds=0.3))
            worker.start()
            try:
                handoff.wait()
                self.assertEqual(len(FakeRuntime.instances[0].calls), 1)
                self.assertEqual(self.state()["ready"]["cards"], cards)
                shown = reader.consume(self.config, event)
            finally:
                reader.end_session(self.config, "s1")
                handoff.finish()
                worker.join(2)
            self.assertFalse(worker.is_alive())
        self.assertEqual(shown["cards"], cards)
        self.assertEqual(len(shown["displayed"]), 8)
        self.assertEqual(len(self.state()["emitted"]), 8)
        self.assertLessEqual(len(shown["context"].encode()), 4096)
        self.assertEqual(reader.consume(self.config, event)["outcome"], "empty")

    def test_budget_omitted_card_not_emitted_and_remains_eligible_next_turn(self):
        oversized = {**CARD_A, "kind": "episode", "episode_id": CARD_A["id"],
                     "edition_id": CARD_A["id"], "revision": 1,
                     "current_edition_id": CARD_A["id"], "occurred": {"kind": "point", "at": 12},
                     "recorded_at": 15, "edition_recorded_at": 20, "thread": "é" * 2300,
                     "recording_session": None, "origins": [{"kind": "lexical"}]}
        event = self.event()
        reader.notice(self.config, event, True)
        self.set_state(lambda data: data.update(ready={**data["pending"], "cards": [oversized, CARD_B],
                                                      "fence": data["fence"]}))
        shown = reader.consume(self.config, event)
        self.assertEqual(shown["cards"], [CARD_B])
        self.assertEqual(shown["budget_omitted_count"], 1)
        self.assertIn("Selected-batch", shown["context"])
        self.assertEqual(self.state()["emitted"], [[CARD_B["id"], CARD_B["fingerprint"]]])
        self.assertEqual(self.state()["last_delivery"]["budget_omitted_count"], 1)
        self.assertEqual(self.state()["counts"]["failed"], 0)
        self.assertEqual(reader.consume(self.config, event)["outcome"], "empty")
        reader.notice(self.config, self.event("t2"), True)
        smaller = {**oversized, "thread": "short"}
        self.set_state(lambda data: data.update(ready={**data["pending"], "cards": [smaller],
                                                      "fence": data["fence"]}))
        self.assertEqual(reader.consume(self.config, self.event("t2"))["cards"], [smaller])

    def test_foreign_duplicate_or_nonstring_selector_ids_refused(self):
        runtime = FakeRuntime(self.config, self.root)
        for ids in ([CARD_A["id"], CARD_A["id"]], [CARD_B["id"]], [{}], [CARD_A["id"], CARD_B["id"]]):
            with self.subTest(ids=ids), patch("hook_recall.collect_reader", return_value={"outcome": "ok", "cards": [CARD_A]}):
                runtime.result = {**FakeRuntime.result, "selected_ids": ids}
                result = reader._execute(self.config, "Current substantive task", runtime, lambda: True, set())
                self.assertEqual(result["cards"], [])

    def test_selection_diagnostic_pool_is_postpack_unseen_and_content_free(self):
        cards = [{**CARD_A, "id": "01ARZ3NDEKTSV4RRFFQ69G5FA" + str(i)} for i in range(9)]
        runtime = FakeRuntime(self.config, self.root)
        runtime.result = {**FakeRuntime.result, "selected_ids": [cards[7]["id"]]}
        with patch("hook_recall.collect_reader", return_value={"outcome": "ok", "cards": cards}):
            result = reader._execute(self.config, "private current task", runtime, lambda: True,
                                     {(cards[0]["id"], cards[0]["fingerprint"])})
        diagnostic = reader._selection_diagnostic(result, {"turn": "t1"}, True)
        self.assertEqual(diagnostic["pool_stage"], "postpack_unseen")
        self.assertEqual(diagnostic["pool"], [{"id": c["id"], "fingerprint": c["fingerprint"]} for c in cards[1:]])
        self.assertEqual(diagnostic["selected_ids"], [cards[7]["id"]])
        self.assertEqual(diagnostic["seen_count"], 1)
        self.assertFalse(diagnostic["postpack_truncated"])
        self.assertIn(cards[8]["id"], json.dumps(diagnostic))
        for forbidden in ("summary", "source", "private current task", "graph_path", "body"):
            self.assertNotIn(forbidden, json.dumps(diagnostic))
        diagnostic["pool"][0]["fingerprint"] = "diagnostic-only"
        self.assertEqual(cards[1]["fingerprint"], "a")
        self.assertEqual(result["cards"], [cards[7]])

    def test_selection_diagnostic_distinguishes_empty_unavailable_invalid_and_seen(self):
        cases = [(None, set(), None, "native"),
                 ({"outcome": "empty", "cards": []}, set(), [], "native"),
                 ({"outcome": "ok", "cards": [{}]}, set(), [], "filter"),
                 ({"outcome": "ok", "cards": [CARD_A]}, {(CARD_A["id"], "a")}, [], "filter")]
        for native, seen, pool, stage in cases:
            with self.subTest(native=native), patch("hook_recall.collect_reader", return_value=native):
                runtime = FakeRuntime(self.config, self.root)
                result = reader._execute(self.config, "task", runtime, lambda: True, seen)
                trace = reader._selection_diagnostic(result, {}, True)
                self.assertEqual(trace["pool"], pool)
                self.assertEqual(trace["stage"], stage)
                self.assertEqual(trace["selected_ids"], [])
                self.assertEqual(runtime.calls, [])
        self.assertEqual(trace["seen_count"], 1)

    def test_selection_diagnostic_survives_budget_exception_and_invalid_selection(self):
        for reason in ("budget", "exception", "foreign_id", "abstained", "usage_unknown"):
            with self.subTest(reason=reason), patch("hook_recall.collect_reader", return_value={"outcome": "ok", "cards": [CARD_A]}):
                runtime = FakeRuntime(self.config, self.root)
                runtime.result = {**FakeRuntime.result, "selected_ids": [], "reason": reason}
                if reason == "foreign_id":
                    runtime.result.update(selected_ids=[CARD_B["id"]], reason="selected")
                if reason == "usage_unknown":
                    runtime.result["usage"] = None
                if reason == "exception":
                    runtime.select = lambda *_: (_ for _ in ()).throw(RuntimeError("provider failed"))
                result = reader._execute(self.config, "task", runtime, lambda: reason != "budget", set())
                trace = reader._selection_diagnostic(result, {}, True)
                self.assertEqual(trace["pool"], [{"id": CARD_A["id"], "fingerprint": "a"}])
                self.assertEqual(trace["selected_ids"], [])
                self.assertEqual(trace["outcome"], "budget" if reason == "budget" else "abstained" if reason == "abstained" else "unavailable")
                if reason in ("exception", "usage_unknown"):
                    self.assertTrue(result["provider_attempt"])
                    self.assertIsNone(result["usage"])

    def test_selection_diagnostic_malformed_optional_data_cannot_override_identity(self):
        origin = {"generation": "a" * 32, "turn": "t1", "request": "b" * 64, "epoch": 1}
        valid = dict(pool_stage="postpack_unseen", pool=[], selected_ids=[], stage="selector")
        malformed = [[], {**valid, "origin": {"turn": "wrong"}}, {**valid, "body": "private"},
                     {**valid, "pool": object()}, {**valid, "selected_ids": [CARD_A["id"]]},
                     {**valid, "native_outcome": "\ud800"}]
        for details in malformed:
            with self.subTest(details=repr(details)):
                trace = reader._selection_diagnostic({"outcome": "abstained", "selection": details}, origin, False)
                self.assertEqual(trace, {"schema": 1, "origin": origin, "publish_current": False,
                                         "outcome": "abstained", "omitted": "invalid_diagnostic"})
        large_pool = [{"id": CARD_A["id"], "fingerprint": "🦋" * 160} for _ in range(8)]
        trace = reader._selection_diagnostic({"outcome": "abstained", "selection": {**valid, "pool": large_pool}}, origin, True)
        self.assertEqual(trace["omitted"], "byte_cap")
        self.assertLess(len(json.dumps(trace).encode()), reader.MAX_SELECTION_BYTES)

    def test_selection_diagnostic_pressure_sheds_trace_before_useful_ready(self):
        reader.notice(self.config, self.event(), True)
        base = self.state()
        ready = {**base["pending"], "cards": [CARD_A], "fence": base["fence"]}
        expected = {**base, "ready": ready, "input_tokens": 13, "output_tokens": 3}
        cap = len(json.dumps(expected, ensure_ascii=False, separators=(",", ":")).encode()) + 40
        with patch.object(reader, "MAX_STATE", cap):
            self.set_state(lambda data: data.update(ready=ready, input_tokens=13, output_tokens=3,
                                                     last_selection={"extra": "x" * 500}))
        state = self.state()
        self.assertNotIn("last_selection", state)
        self.assertEqual(state["ready"], ready)
        self.assertEqual((state["input_tokens"], state["output_tokens"]), (13, 3))
        self.assertEqual(state["counts"]["dropped"], 0)

    def test_discovery_is_private_and_does_not_change_selector_payload(self):
        from hook_recall import _discovery
        from test_hook_recall import discovery_metadata
        metadata = discovery_metadata()
        metadata["omitted"]["expansion"]["further_tail_unknown"] = True
        discovery = _discovery(metadata, {"observed_unique_window": 2, "readbacks_attempted": 2, "work_budget_unread": 0, "graph_readback_omitted": 0, "graph_byte_omitted": 0,
                                         "readback_skipped": 0, "byte_budget_omitted": 0, "returned": 2})
        runtime = FakeRuntime(self.config, self.root)
        cue = "Current task: retain exact memory policy"
        native = {"outcome": "ok", "cards": [CARD_A, CARD_B]}
        with patch("hook_recall.collect_reader", return_value=native):
            baseline = reader._execute(self.config, cue, runtime, lambda: True, set())
        baseline_payload = json.dumps(runtime.calls[-1], ensure_ascii=False, separators=(",", ":")).encode()
        with patch("hook_recall.collect_reader", return_value={**native, "discovery": discovery}):
            covered = reader._execute(self.config, cue, runtime, lambda: True, set())
        self.assertEqual(json.dumps(runtime.calls[-1], ensure_ascii=False, separators=(",", ":")).encode(), baseline_payload)
        self.assertEqual(covered["cards"], baseline["cards"])
        self.assertEqual(covered["usage"], baseline["usage"])
        self.assertEqual(covered["selection"]["discovery"], discovery)
        diagnostic = reader._selection_diagnostic(covered, {"turn": "t1", "epoch": 1}, False)
        self.assertEqual(diagnostic["discovery"], discovery)
        self.assertFalse(diagnostic["publish_current"])
        self.assertNotIn(cue, json.dumps(diagnostic))
        self.assertNotIn(CARD_A["summary"], json.dumps(diagnostic))
        self.assertNotIn("stamp", json.dumps(diagnostic))

    def test_selection_byte_pressure_sheds_discovery_before_valid_pool(self):
        from hook_recall import _discovery
        from test_hook_recall import discovery_metadata
        metadata = discovery_metadata()
        metadata["retrieval"].update(mode="tagged", partial=False,
            work={key: 0 for key in __import__("hook_recall")._TAGGED_WORK_FIELDS})
        metadata["retrieval"]["lanes"]["primary"]["seed_coverage"] = {"strategy": "exact_cosine"}
        discovery = _discovery(metadata, {"observed_unique_window": 8, "readbacks_attempted": 2, "work_budget_unread": 0, "graph_readback_omitted": 0, "graph_byte_omitted": 0,
                                         "readback_skipped": 0, "byte_budget_omitted": 0, "returned": 8})
        pool = [{"id": "01ARZ3NDEKTSV4RRFFQ69G5FA" + str(i), "fingerprint": "a" * 160} for i in range(8)]
        details = dict(pool_stage="postpack_unseen", pool=pool, selected_ids=[p["id"] for p in pool],
                       stage="selector", discovery=discovery)
        origin = {"session": "s" * 160, "turn": "t" * 160, "generation": "a" * 32,
                  "request": "b" * 64, "epoch": 1, "context_generation": 1,
                  "context_token": "c" * 32, "fence": "d" * 32}
        result = {"outcome": "selected", "selection": details}
        raw = {"schema": 1, "origin": origin, "publish_current": True, "outcome": "selected", **details}
        cap = len(json.dumps(raw, ensure_ascii=False, separators=(",", ":")).encode()) - 100
        with patch.object(reader, "MAX_SELECTION_BYTES", min(cap, 4096)):
            diagnostic = reader._selection_diagnostic(result, origin, True)
        self.assertEqual(diagnostic["pool"], pool)
        self.assertEqual(diagnostic["selected_ids"], details["selected_ids"])
        self.assertEqual(diagnostic["discovery"]["native"], {"state": "unknown", "reason": "diagnostic_byte_cap"})
        self.assertLessEqual(len(json.dumps(diagnostic, ensure_ascii=False, separators=(",", ":")).encode()), min(cap, 4096))

    def test_state_pressure_sheds_only_discovery_before_ready_and_usage(self):
        from hook_recall import _discovery, _unknown_discovery
        from test_hook_recall import discovery_metadata
        reader.notice(self.config, self.event(), True)
        base = self.state()
        ready = {**base["pending"], "cards": [CARD_A], "fence": base["fence"]}
        diagnostic = {"pool_stage": "postpack_unseen", "pool": [], "selected_ids": [], "stage": "selector",
                      "discovery": _discovery(discovery_metadata(), None)}
        expected = {**base, "ready": ready, "input_tokens": 13, "output_tokens": 3,
                    "last_selection": {**diagnostic, "discovery": _unknown_discovery("state_byte_cap")}}
        cap = len(json.dumps(expected, ensure_ascii=False, separators=(",", ":")).encode()) + 40
        with patch.object(reader, "MAX_STATE", cap):
            self.set_state(lambda data: data.update(ready=ready, input_tokens=13, output_tokens=3,
                                                     last_selection=diagnostic))
        state = self.state()
        self.assertEqual(state["last_selection"]["discovery"]["native"]["reason"], "state_byte_cap")
        self.assertEqual(state["ready"], ready)
        self.assertEqual((state["input_tokens"], state["output_tokens"]), (13, 3))
        self.assertEqual(state["counts"]["dropped"], 0)

    def test_selection_diagnostic_malformed_previous_slot_does_not_break_accounting(self):
        for index, previous in enumerate((None, [], {"origin": None}, {"origin": []},
                                           {"origin": {"epoch": "bad"}})):
            with self.subTest(previous=previous):
                reader.notice(self.config, self.event("t" + str(index)), True)
                self.set_state(lambda data: data.update(last_selection=previous))
                with patch("hook_recall.collect_reader", return_value={"outcome": "ok", "cards": [CARD_A]}):
                    reader.serve(self.config, "s1", runtime_factory=FakeRuntime, idle_seconds=.01)
                state = self.state()
                self.assertEqual(state["input_tokens"], 10 * (index + 1))
                self.assertEqual(state["counts"]["ready"], index + 1)
                self.assertTrue(state["last_selection"]["publish_current"])
                self.assertEqual(state["last_selection"]["origin"]["turn"], "t" + str(index))

    def test_selection_diagnostic_cancelled_result_keeps_original_identity_and_usage(self):
        for index, cancel in enumerate((lambda: reader.close_turn(self.config, "s1", "t0"),
                                       lambda: reader.reset(self.config, "s1"),
                                       lambda: reader.notice(self.config, self.event("new", "ok"), False))):
            with self.subTest(index=index):
                reader.notice(self.config, self.event("t" + str(index)), True)
                before = self.state()
                class CancellingRuntime(FakeRuntime):
                    def select(inner, dialogue, cards):
                        cancel()
                        return super().select(dialogue, cards)
                with patch("hook_recall.collect_reader", return_value={"outcome": "ok", "cards": [CARD_A]}):
                    reader.serve(self.config, "s1", runtime_factory=CancellingRuntime, idle_seconds=.01)
                state = self.state()
                trace = state["last_selection"]
                self.assertFalse(trace["publish_current"])
                self.assertEqual(trace["origin"]["turn"], "t" + str(index))
                for key in ("generation", "context_generation", "epoch", "fence"):
                    self.assertEqual(trace["origin"][key], before[key])
                self.assertEqual(trace["origin"]["request"], before["pending"]["request"])
                self.assertIsNone(state["ready"])
                self.assertEqual(state["input_tokens"], 10 * (index + 1))
                self.assertNotIn("selection", reader.consume(self.config, self.event("t" + str(index))))

    def test_older_selection_does_not_replace_newer_completion_but_settles_usage(self):
        reader.notice(self.config, self.event(), True)
        before = self.state()
        newer = {"schema": 1, "origin": {"generation": before["generation"],
                 "turn": "newer", "epoch": before["epoch"] + 1}, "outcome": "abstained"}
        self.set_state(lambda data: data.update(last_selection=newer))
        owner = self
        class LateRuntime(FakeRuntime):
            def select(inner, dialogue, cards):
                reader.close_turn(owner.config, "s1", "t1")
                return super().select(dialogue, cards)
        with patch("hook_recall.collect_reader", return_value={"outcome": "ok", "cards": [CARD_A]}):
            reader.serve(self.config, "s1", runtime_factory=LateRuntime, idle_seconds=.01)
        state = self.state()
        self.assertEqual(state["last_selection"], newer)
        self.assertEqual((state["input_tokens"], state["output_tokens"]), (10, 2))
        self.assertIsNone(state["reservation"])
        self.assertIsNone(state["ready"])

    def test_reader_gets_actual_graph_path_but_ready_stays_original(self):
        path = [{"previous": CARD_A["id"], "target": CARD_B["id"],
                 "from": CARD_A["id"], "to": CARD_B["id"],
                 "kind": "contradicts", "anchor": CARD_A["id"]}]
        native = {"outcome": "ok", "cards": [CARD_A],
                  "observation": {"schema": 1, "learning": "disabled",
                                  "cards": [{"node_id": CARD_A["id"], "graph_path": path}]}}
        runtime = FakeRuntime(self.config, self.root)
        with patch("hook_recall.collect_reader", return_value=native):
            result = reader._execute(self.config, "Current task: check contradiction", runtime,
                                     lambda: True, set())
        self.assertEqual(runtime.calls[0][1][0]["native"]["graph_path"], path)
        self.assertEqual(result["cards"], [CARD_A])
        self.assertNotIn("native", result["cards"][0])

    def test_previously_emitted_candidates_filtered_before_model(self):
        runtime = FakeRuntime(self.config, self.root)
        fresh = {**CARD_A, "id": "01K123456789ABCDEFGHJKMNPS", "fingerprint": "fresh"}
        FakeRuntime.result = {"selected_ids": [fresh["id"]], "reason": "selected",
                              "usage": {"input_tokens": 3, "output_tokens": 1},
                              "elapsed_ms": 1, "provider_attempt": True}
        native = {"outcome": "ok", "cards": [CARD_A, CARD_B, fresh]}
        with patch("hook_recall.collect_reader", return_value=native):
            result = reader._execute(self.config, "Current task: something else", runtime,
                                     lambda: True, {(CARD_A["id"], CARD_A["fingerprint"]),
                                                    (CARD_B["id"], CARD_B["fingerprint"])})
        self.assertEqual([c["id"] for c in runtime.calls[0][1]], [fresh["id"]])
        self.assertEqual(result["cards"], [fresh])
        # A changed fingerprint must remain eligible.
        with patch("hook_recall.collect_reader", return_value={"outcome": "ok", "cards": [{**CARD_A, "fingerprint": "changed"}]}):
            reader._execute(self.config, "Current task: changed memory", runtime,
                            lambda: True, {(CARD_A["id"], CARD_A["fingerprint"])})
        self.assertEqual(runtime.calls[-1][1][0]["fingerprint"], "changed")

    def test_reset_rotates_runtime_without_resetting_ledger(self):
        self.assertEqual(reader.notice(self.config, self.event(), True)["outcome"], "queued")
        with patch("hook_recall.collect_reader", return_value={"outcome": "ok", "cards": [CARD_A]}), \
             PublicationHandoff(reader, "s1") as handoff:
            worker = threading.Thread(target=lambda: reader.serve(self.config, "s1", runtime_factory=FakeRuntime, idle_seconds=5))
            worker.start()
            try:
                first = handoff.wait()
                before = self.state()
                self.assertEqual((before["attempts"], before["input_tokens"], before["output_tokens"]), (1, 10, 2))
                self.assertIsNotNone(before["ready"])
                reader.reset(self.config, "s1")
                self.assertEqual(self.state()["context_generation"], before["context_generation"] + 1)
                self.assertEqual(reader.notice(self.config, self.event("t2", "Another substantial task"), True)["outcome"], "queued")
                first.set()
                handoff.wait()
                self.assertEqual((self.state()["input_tokens"], self.state()["output_tokens"]), (20, 4))
            finally:
                reader.end_session(self.config, "s1")
                handoff.finish()
                worker.join(2)
            self.assertFalse(worker.is_alive())
        self.assertEqual(self.state()["attempts"], 2)
        self.assertGreaterEqual(len(FakeRuntime.instances), 2)
        self.assertTrue(all(x.closed for x in FakeRuntime.instances))


    def test_recording_uses_shared_reservation_and_settles_only_once(self):
        reader.notice(self.config, self.event(), False)
        self.assertTrue(reader._recording_reserve(self.config, "s1", "job"))
        self.assertFalse(reader._recording_reserve(self.config, "s1", "second"))
        result = {"provider_attempt": True, "usage": {"input_tokens": 9, "output_tokens": 3,
                  "cached_input_tokens": 7, "reasoning_output_tokens": 2}}
        self.assertTrue(reader._recording_account(self.config, "s1", "job", result))
        self.assertFalse(reader._recording_account(self.config, "s1", "job", result))
        self.assertEqual((self.state()["attempts"], self.state()["input_tokens"], self.state()["output_tokens"]), (1, 9, 3))
        self.assertEqual((self.state()["cached_input_tokens"], self.state()["reasoning_output_tokens"]), (7, 2))
        self.assertTrue(reader._recording_reserve(self.config, "s1", "unknown"))
        reader._recording_account(self.config, "s1", "unknown", {"provider_attempt": True, "usage": None})
        self.assertTrue(self.state()["unknown_usage"])
        self.assertIsNone(self.state()["cached_input_tokens"])
        self.assertIsNone(self.state()["reasoning_output_tokens"])
        self.assertFalse(reader._recording_reserve(self.config, "s1", "later"))

    def test_breakdown_missing_invalid_or_historical_unknown_never_becomes_zero(self):
        reader.notice(self.config, self.event(), False)
        self.assertEqual((self.state()["cached_input_tokens"], self.state()["reasoning_output_tokens"]), (0, 0))
        for index, fields in enumerate(({}, {"cached_input_tokens": True, "reasoning_output_tokens": 5},
                                      {"cached_input_tokens": 11, "reasoning_output_tokens": -1},
                                      {"cached_input_tokens": 8, "reasoning_output_tokens": 1})):
            with self.subTest(fields=fields):
                if index < 3:
                    self.set_state(lambda data: data.update(cached_input_tokens=0, reasoning_output_tokens=0))
                self.assertTrue(reader._recording_reserve(self.config, "s1", str(index)))
                self.assertTrue(reader._recording_account(self.config, "s1", str(index),
                    {"provider_attempt": True, "usage": {"input_tokens": 10, "output_tokens": 2, **fields}}))
                self.assertIsNone(self.state()["cached_input_tokens"])
                self.assertIsNone(self.state()["reasoning_output_tokens"])
        self.set_state(lambda data: (data.pop("cached_input_tokens"), data.pop("reasoning_output_tokens")))
        self.assertTrue(reader._recording_reserve(self.config, "s1", "old-ledger"))
        self.assertTrue(reader._recording_account(self.config, "s1", "old-ledger", {
            "provider_attempt": True, "usage": {"input_tokens": 10, "output_tokens": 2,
            "cached_input_tokens": 8, "reasoning_output_tokens": 1}}))
        self.assertIsNone(self.state()["cached_input_tokens"])
        self.assertIsNone(self.state()["reasoning_output_tokens"])
        self.assertFalse(self.state()["unknown_usage"])
        self.assertEqual((self.state()["input_tokens"], self.state()["output_tokens"]), (50, 10))

    def routing_run(self, *, baseline=False, change=None, initial=None):
        import routing_contract
        from test_routing_contract import witness, DB
        self.config['recording_mode'] = 'automatic'
        self.config.pop('_reader_config_pin', None)  # Deliberately load a new fixture config.
        config = self.config
        reader.notice(config, self.event(), True)
        if initial:
            self.set_state(initial)
        owner = self

        class Runtime(FakeRuntime):
            def route(self, current, witnesses, *, expected_db_id):
                state = owner.state()
                owner.assertIn('routing', state['reservation'])
                owner.assertEqual(state['routing_usage']['attempts'], 1)
                _, context = routing_contract.prepare(current, witnesses, expected_db_id=expected_db_id)
                history = json.loads(context.snapshot_json)['history']
                value = routing_contract.validate_answer({'judgments': [
                    {'witness': w['id'], 'applicability':'matched', 'current_refs':['c1'],
                     'correction_applicable':False}
                    for w in history]}, context)
                result = {'routing':value, 'reason':'matched', 'provider_attempt':True,
                          'usage':{'input_tokens':13,'output_tokens':3,
                                   'cached_input_tokens':4,'reasoning_output_tokens':1}}
                if change:
                    change(config, result)
                return result

        with patch.object(reader, '_baseline_routing', return_value=baseline), \
             patch('hook_recall.collect_routing_witnesses', return_value={
                 'outcome':'ok','db_id':DB,'witnesses':[witness()]}) as discovery, \
             patch('hook_recall.collect_reader', return_value={
                 'outcome':'ok','cards':[CARD_A], 'db_id':DB}) as collect, \
             patch('recording_jobs.has_work', return_value=False), \
             patch.object(reader, 'background', return_value={'outcome':'not_ready'}):
            reader.serve(config, 's1', runtime_factory=Runtime, idle_seconds=.05)
        return discovery, collect, FakeRuntime.instances[-1]

    def test_stale_routing_settlement_retires_only_old_inflight(self):
        hook = self.root / "hook.json"
        hook.write_text("policy A")
        self.config["_config_path"] = hook
        holder = {}
        def changed(config,result):
            hook.write_text("policy B")
            new = {**config,"librarian_effort":"high"}
            new.pop("_reader_config_pin",None)
            holder["new"] = new
            self.assertEqual(reader.notice(new,self.event("next"),True)["outcome"],"queued")
        _,_,runtime = self.routing_run(change=changed)
        state = self.state()
        self.assertEqual(len(runtime.calls),0)
        self.assertIsNone(state["reservation"])
        self.assertIsNone(state["inflight"])
        self.assertEqual(state["pending"]["turn"],"next")
        self.assertEqual((state["attempts"],state["input_tokens"],state["output_tokens"]),(1,13,3))
        new = holder["new"]
        with patch("hook_recall.collect_reader",return_value={"outcome":"ok","cards":[CARD_A]}), \
             patch("recording_jobs.has_work",return_value=False), \
             patch.object(reader,"_baseline_routing",return_value=True):
            reader.serve(new,"s1",runtime_factory=FakeRuntime,idle_seconds=.05)
        self.assertFalse(self.state()["unknown_usage"])
        self.assertEqual(self.state()["input_tokens"],23)

    def test_optional_matching_room_resolves_requested_effort(self):
        from librarian_policy import LibrarianBudget
        for effort,headroom,matched in (("low",10000,True),("high",20000,False)):
            with self.subTest(effort=effort):
                self.config["librarian_effort"] = effort
                policy = LibrarianBudget(effort=effort)
                discovery,_,_ = self.routing_run(initial=lambda d:d.update(input_tokens=policy.input_tokens-headroom))
                self.assertEqual(discovery.call_count,1)  # Native witness read stays bounded separately.
                self.assertEqual(self.state()["attempts"],2 if matched else 1)
                for path in reader._directory(self.config).glob("*"):
                    if path.is_file(): path.unlink()
                FakeRuntime.instances.clear()

    def test_routing_preflight_uses_one_policy_window_and_retains_unknown_work(self):
        from librarian_policy import resolve
        from hook_recall import _bounded_routing_discovery
        from test_routing_contract import witness,DB
        class Runtime:
            def route(self,current,witnesses,*,expected_db_id):
                import routing_contract
                _,ctx = routing_contract.prepare(current,witnesses,expected_db_id=expected_db_id,
                                                  budget=resolve(self_config))
                from test_routing_contract import answer
                return {"routing":routing_contract.validate_answer(answer(ctx),ctx),"reason":"matched",
                        "provider_attempt":True,"usage":{"input_tokens":5,"output_tokens":1}}
        self_config = self.config
        metadata = _bounded_routing_discovery(None)
        with patch("hook_recall.collect_routing_witnesses",return_value={"outcome":"ok","db_id":DB,
                  "witnesses":[witness()],"routing_discovery":metadata}) as collect:
            result = reader._match_routing(self.config,"Inspect scope",Runtime(),lambda:True,lambda result:True)
        kwargs = collect.call_args.kwargs
        self.assertEqual(kwargs["current"],[{"source":"bounded_task_cue","text":"Inspect scope"}])
        self.assertEqual(kwargs["budget"],resolve(self.config))
        self.assertEqual(kwargs["timeout"],resolve(self.config).native_seconds)
        self.assertGreater(kwargs["discovery_plan"]["max_nodes"],8)
        self.assertEqual(result["routing_discovery"],metadata)
        self.assertEqual(result["mode"],"matched")
        with patch("hook_recall.collect_routing_witnesses") as native:
            refused = reader._match_routing(self.config,"x"*6000,Runtime(),lambda:True,lambda result:True)
        native.assert_not_called()
        self.assertEqual(refused["mode"],"input_refused")

    def test_matcher_and_selector_are_reserved_and_charged_exactly_once(self):
        discovery, collect, runtime = self.routing_run()
        self.assertEqual(discovery.call_count, 1)
        self.assertEqual(len(collect.call_args.kwargs['routing_hints']), 1)
        self.assertEqual(len(runtime.calls), 1)
        state = self.state()
        self.assertEqual((state['attempts'], state['input_tokens'], state['output_tokens']), (2,23,5))
        self.assertEqual(state['routing_usage'], {'attempts':1,'input_tokens':13,'output_tokens':3,
            'cached_input_tokens':4,'reasoning_output_tokens':1,'unknown_usage':False})
        self.assertEqual(state['counts']['selected'], 1)
        self.assertEqual(state['counts']['failed'], 0)
        self.assertEqual(state['routing_mode'], 'matched')
        self.assertIsNone(state['reservation'])

    def test_last_call_goes_to_selector_not_optional_matching(self):
        _, collect, runtime = self.routing_run(initial=lambda d: d.update(attempts=reader.MAX_ATTEMPTS-1))
        self.assertEqual(collect.call_args.kwargs['routing_hints'], [])
        self.assertEqual(len(runtime.calls), 1)
        state = self.state()
        self.assertEqual(state['attempts'], reader.MAX_ATTEMPTS)
        self.assertNotIn('routing_usage', state)
        self.assertEqual(state['input_tokens'], 10)

    def test_near_token_limit_keeps_the_selector_not_matching(self):
        for field, value in (('input_tokens', reader.MAX_INPUT-100),
                             ('output_tokens', reader.MAX_OUTPUT-100)):
            with self.subTest(field=field):
                self.config['state_dir'] = self.root / field
                _, collect, runtime = self.routing_run(initial=lambda d: d.update({field:value}))
                self.assertEqual(collect.call_args.kwargs['routing_hints'], [])
                self.assertEqual(len(runtime.calls), 1)
                self.assertNotIn('routing_usage', self.state())
                self.assertEqual(self.state()['attempts'], 1)

    def test_known_invalid_matching_output_is_charged_then_uses_baseline(self):
        _, collect, runtime = self.routing_run(change=lambda _c,r: r.update(routing=None, reason='invalid_output'))
        self.assertEqual(collect.call_args.kwargs['routing_hints'], [])
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(self.state()['input_tokens'], 23)
        self.assertEqual(self.state()['routing_usage']['input_tokens'], 13)
        self.assertEqual(self.state()['routing_mode'], 'matching_refused')

    def test_failed_matching_settlement_does_not_buy_a_second_provider_call(self):
        original = reader._state
        def busy_settlement(config, session, mutate, **kwargs):
            if mutate.__name__ == 'settle':
                return False, None
            return original(config, session, mutate, **kwargs)
        with patch.object(reader, '_state', side_effect=busy_settlement):
            _, collect, runtime = self.routing_run()
        collect.assert_not_called()
        self.assertEqual(runtime.calls, [])
        state = self.state()
        self.assertEqual(state['attempts'], 1)
        self.assertTrue(state['unknown_usage'])  # Abandoned reservation recovered conservatively.
        self.assertTrue(state['routing_usage']['unknown_usage'])
        self.assertIsNone(state['reservation'])

    def test_recording_off_preserves_absent_hint_field_and_skips_matching(self):
        reader.notice(self.config, self.event(), True)
        with patch('hook_recall.collect_routing_witnesses') as discover, \
             patch('hook_recall.collect_reader', return_value={'outcome':'ok', 'cards':[CARD_A]}) as collect:
            reader.serve(self.config, 's1', runtime_factory=FakeRuntime, idle_seconds=.05)
        discover.assert_not_called()
        self.assertNotIn('routing_hints', collect.call_args.kwargs)
        self.assertNotIn('routing_usage', self.state())
        self.assertNotIn('routing_mode', self.state())
        self.assertEqual(self.state()['attempts'], 1)

    def test_unknown_matching_usage_stops_selector_and_is_not_retried(self):
        _, collect, runtime = self.routing_run(change=lambda _c,r: r.update(usage=None))
        collect.assert_not_called()
        self.assertEqual(runtime.calls, [])
        state = self.state()
        self.assertEqual(state['attempts'], 1)
        self.assertTrue(state['unknown_usage'])
        self.assertTrue(state['routing_usage']['unknown_usage'])
        self.assertEqual(state['routing_mode'], 'usage_unresolved')

    def test_cancelled_match_cost_is_settled_but_selector_does_not_run(self):
        _, _, runtime = self.routing_run(change=lambda c,_r: reader.close_turn(c,'s1','t1'))
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.state()['input_tokens'], 13)
        self.assertEqual(self.state()['attempts'], 1)
        self.assertFalse(self.state()['unknown_usage'])

    def test_generation_change_does_not_erase_matching_usage(self):
        def rotate(config, result):
            reader.end_session(config, 's1')
            reader.reset(config, 's1')
        _, _, runtime = self.routing_run(change=rotate)
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.state()['input_tokens'], 13)
        self.assertEqual(self.state()['routing_usage']['input_tokens'], 13)
        self.assertFalse(self.state()['unknown_usage'])
        self.assertIsNone(self.state()['reservation'])

    def test_baseline_opportunity_skips_discovery_and_matching(self):
        discovery, collect, runtime = self.routing_run(baseline=True)
        discovery.assert_not_called()
        self.assertEqual(collect.call_args.kwargs['routing_hints'], [])
        self.assertEqual(self.state()['attempts'], 1)
        self.assertEqual(self.state()['routing_mode'], 'baseline_exploration')
        self.assertNotIn('routing_usage', self.state())
        self.assertEqual(len(runtime.calls), 1)

    def test_baseline_hash_ignores_epoch_and_process_replay(self):
        pending = {'turn':'opaque-turn','request':'a'*64, 'epoch':1}
        first = reader._baseline_routing('s1',pending)
        self.assertEqual(first, reader._baseline_routing('s1',{**pending,'epoch':99}))
        choices = {reader._baseline_routing('s1',{**pending,'turn':str(i)}) for i in range(100)}
        self.assertEqual(choices, {False,True})

    def test_empty_recall_retains_small_routing_omission_diagnostic(self):
        with patch('hook_recall.collect_reader', return_value={'outcome':'empty', 'cards':[]}):
            result = reader._execute(self.config, 'current task', FakeRuntime(self.config, self.root),
                lambda:True, set(), routing=lambda:{'mode':'neutral', 'hints':[],
                                                    'omitted_groups':2, 'stop':False})
        self.assertEqual(result['routing_mode'], 'neutral')
        self.assertEqual(result['routing_omitted_groups'], 2)
        self.assertFalse(result['provider_attempt'])

    def test_end_session_drains_recording_using_existing_worker_and_budget(self):
        config = {**self.config, "recording_mode": "automatic"}
        reader.notice(config, self.event(), False)
        pending = [True]
        seen = []
        def step(_config, session, runtime, reserve, account):
            self.assertEqual(session, "s1")
            self.assertTrue(reserve("closed-job"))
            self.assertTrue(account("closed-job", {"provider_attempt": True,
                           "usage": {"input_tokens": 12, "output_tokens": 2}}))
            seen.append(runtime)
            pending[0] = False
            return True
        with patch("recording_jobs.close_turn") as close, \
             patch("recording_jobs.has_work", side_effect=lambda *_: pending[0]), \
             patch("recording_jobs.step", side_effect=step):
            reader.end_session(config, "s1")
            close.assert_called_once_with(config, "s1", ended=True)
            reader.serve(config, "s1", runtime_factory=FakeRuntime, idle_seconds=.1)
        self.assertEqual(len(seen), 1)
        self.assertEqual(self.state()["attempts"], 1)
        self.assertEqual(self.state()["input_tokens"], 12)
        self.assertIsNone(self.state()["ready"])

    def test_generation_handoff_releases_lease_and_accounts_without_stale_delivery(self):
        for unknown in (False, True):
            with self.subTest(unknown=unknown):
                # Fresh isolated ledger per case; no provider or process launch.
                config = {**self.config, "state_dir": self.root / ("unknown" if unknown else "known"),
                          "recording_mode": "automatic"}
                reader.notice(config, self.event(), True)
                FakeRuntime.result["usage"] = {"input_tokens": 10, "output_tokens": 2,
                                               "cached_input_tokens": 7, "reasoning_output_tokens": 1}
                entered, release = threading.Event(), threading.Event()
                calls, handoffs = [], []
                class Runtime(FakeRuntime):
                    def select(self, dialogue, cards):
                        calls.append(dialogue)
                        if len(calls) == 1:
                            entered.set()
                            self_outer.assertTrue(release.wait(2))
                            if unknown:
                                return {"selected_ids": [], "reason": "usage_unknown",
                                        "provider_attempt": True, "usage": None}
                        return dict(FakeRuntime.result)
                self_outer = self
                def handoff(_config, event):
                    handoffs.append(event)
                    reader.serve(config, "s1", runtime_factory=Runtime, idle_seconds=.05)
                    return {"outcome": "launched"}
                original_state = reader._state
                def repeat_publish(*args, **kwargs):
                    value = original_state(*args, **kwargs)
                    if args[2].__name__ == "publish":
                        original_state(*args, **kwargs)
                    return value
                with patch("hook_recall.collect_reader", return_value={"outcome": "ok", "cards": [CARD_A]}), \
                     patch("recording_jobs.close_turn"), patch("recording_jobs.has_work", return_value=False), \
                     patch.object(reader, "background", side_effect=handoff), \
                     patch.object(reader, "_state", side_effect=repeat_publish):
                    worker = threading.Thread(target=lambda: reader.serve(config, "s1", runtime_factory=Runtime, idle_seconds=.2))
                    worker.start()
                    self.assertTrue(entered.wait(1))
                    reader.end_session(config, "s1")
                    reader.reset(config, "s1")
                    reader.notice(config, self.event("t2", "A genuinely new task"), True)
                    # A current-generation wake cannot take the old held lease.
                    reader.serve(config, "s1", runtime_factory=Runtime, idle_seconds=.01)
                    self.assertEqual(len(calls), 1)
                    release.set(); worker.join(3)
                    self.assertFalse(worker.is_alive())
                data = json.loads(reader._paths(config, "s1")[0].read_text())
                self.assertEqual(len(handoffs), 1)
                self.assertEqual(data["attempts"], 1 if unknown else 2)
                self.assertEqual(data["input_tokens"], 0 if unknown else 20)
                self.assertEqual(data["unknown_usage"], unknown)
                self.assertEqual(data["cached_input_tokens"], None if unknown else 14)
                self.assertEqual(data["reasoning_output_tokens"], None if unknown else 2)
                if data["ready"] is not None:
                    self.assertEqual(data["ready"]["turn"], "t2")


class ReaderConcernTests(unittest.TestCase):
    setUp = ReaderStateTests.setUp
    event = ReaderStateTests.event
    set_state = ReaderStateTests.set_state
    state = ReaderStateTests.state

    def test_notice_after_durable_usage_then_once_only_bound_delivery(self):
        self.config["recording_mode"] = "automatic"
        from test_turn_observer import concern_row
        row = concern_row(CARD_A["id"], CARD_B["id"])
        nomination = {"kind": "disagreement", "left_id": CARD_A["id"], "right_id": CARD_B["id"],
                      "caveat": "Advice differs", "missing_fact": "Which scope?"}
        class Runtime(FakeRuntime):
            def select(self, dialogue, cards, **kwargs):
                return {**self.result, "selected_ids": [CARD_A["id"], CARD_B["id"]], "concerns": [nomination]}
        native = {"outcome": "ok", "cards": [{**CARD_A, "fingerprint": "a" * 64}, {**CARD_B, "fingerprint": "b" * 64}], "db_id": PROJECT_DB,
                  "concern_endpoints": {e["id"]: e for e in row["notice"]["binding"]["endpoints"]}}
        def register(*args, **kwargs):
            self.assertEqual(self.state()["input_tokens"], 10)
            self.assertIsNone(self.state()["reservation"])
            self.assertTrue(kwargs["allowed"]())
            return [{"shown_text": "Advice differs. Which scope?", "displayed_endpoint_ids": [CARD_A["id"], CARD_B["id"]],
                     "expected_row": row}]
        reader.notice(self.config, self.event(), True)
        with patch("hook_recall.collect_reader", return_value=native), patch("hook_recall.register_reader_concerns", side_effect=register), \
             PublicationHandoff(reader, "s1") as handoff:
            worker = threading.Thread(target=lambda: reader.serve(self.config, "s1", runtime_factory=Runtime, idle_seconds=.5))
            worker.start()
            try:
                handoff.wait()
                result = reader.consume(self.config, self.event())
            finally:
                reader.end_session(self.config, "s1")
                handoff.finish()
                worker.join(2)
            self.assertFalse(worker.is_alive())
        self.assertEqual(len(result["concerns"]), 1)
        self.assertIn("Advice differs. Which scope?", result["context"])
        self.assertEqual(self.state()["last_delivery"]["registered_concern_count"], 1)
        self.assertEqual(reader.consume(self.config, self.event())["cards"], [])

    def test_invalid_pair_does_not_acquire_maintenance_authority(self):
        class Runtime:
            def select(self, *args, **kwargs):
                return {"selected_ids": [CARD_A["id"]], "reason": "selected", "concerns": [
                    {"kind": "disagreement", "left_id": CARD_A["id"], "right_id": CARD_B["id"],
                     "caveat": "bad", "missing_fact": "scope"}], "provider_attempt": True,
                    "usage": {"input_tokens": 10, "output_tokens": 2}}
        with patch("hook_recall.collect_reader", return_value={"outcome": "ok", "cards": [CARD_A, CARD_B]}):
            result = reader._execute(self.config, "Resolve task", Runtime(), lambda: True, set())
        self.assertEqual(result["cards"], [])
        self.assertEqual(result["concern_nominations"], [])

    def test_recording_off_surfaces_readonly_caveat_without_native_connection(self):
        from test_turn_observer import concern_row
        row = concern_row(CARD_A["id"], CARD_B["id"])
        nomination = {"kind": "disagreement", "left_id": CARD_A["id"], "right_id": CARD_B["id"],
                      "caveat": "Advice differs", "missing_fact": "Which scope?"}
        class Runtime(FakeRuntime):
            def select(self, dialogue, cards, **kwargs):
                return {**self.result, "selected_ids": [CARD_A["id"], CARD_B["id"]], "concerns": [nomination]}
        native = {"outcome": "ok", "cards": [{**CARD_A, "fingerprint": "a" * 64}, {**CARD_B, "fingerprint": "b" * 64}],
                  "db_id": PROJECT_DB, "concern_endpoints": {e["id"]: e for e in row["notice"]["binding"]["endpoints"]}}
        self.config["recording_mode"] = "off"
        reader.notice(self.config, self.event(), True)
        with patch("hook_recall.collect_reader", return_value=native), patch("hook_recall.McpClient") as client, \
             patch("hook_recall._project_config") as project_config, PublicationHandoff(reader, "s1") as handoff:
            worker = threading.Thread(target=lambda: reader.serve(self.config, "s1", runtime_factory=Runtime, idle_seconds=.5))
            worker.start()
            try:
                handoff.wait()
                result = reader.consume(self.config, self.event())
            finally:
                reader.end_session(self.config, "s1")
                handoff.finish()
                worker.join(2)
            self.assertFalse(worker.is_alive())
        client.assert_not_called()
        project_config.assert_not_called()
        self.assertIn("Advice differs", result["context"])
        self.assertEqual(len(result["concerns"]), 1)
        self.assertIsNone(result["concerns"][0]["expected_row"])
        self.assertEqual(self.state()["input_tokens"], 10)


class ConditionalEntryWorkerTests(unittest.TestCase):
    def execute(self, changes=None, *, omit=False, orphan=False):
        from test_routing_memory import binding, TARGET, DB
        route = binding()
        card = {**CARD_A, "id": TARGET}
        row = {"node_id": TARGET, "entry_kind": "conditional", "conditional_binding": route,
               "graph_path": None}
        row.update(changes or {})
        if orphan:
            row.pop("entry_kind")
        native = {"outcome": "ok", "cards": [card], "db_id": DB,
                  "observation": {"schema": 1, "learning": "disabled", "cards": [row]}}
        runtime = FakeRuntime({}, None)
        runtime.result = {"selected_ids": [TARGET], "concerns": [], "reason": "selected",
                          "provider_attempt": True, "usage": {"input_tokens": 10, "output_tokens": 2}}
        config = {"service_config": Path("not-opened"), "project_root": Path("not-opened"),
                  "librarian_effort": "medium", "reader_model": "gpt-6.1-sol"}
        prepare = reader.reader_prepare
        def packed(*args, **kwargs):
            prompt, offered = prepare(*args, **kwargs)
            if omit:
                offered["entry_omitted_ids"] = [TARGET]
            return prompt, offered
        with patch("hook_recall.collect_reader", return_value=native), patch.object(reader, "reader_prepare", packed):
            result = reader._execute(config, "Investigate this repository problem", runtime, lambda: True, set())
        return result, runtime, route

    def test_conditional_marker_reaches_selector_binding_only_reaches_emitted_card(self):
        result, runtime, route = self.execute()
        offered = runtime.calls[0][1][0]
        self.assertEqual(offered["native"], {"entry_kind": "conditional"})
        self.assertNotIn("conditional_binding", offered)
        emitted = result["cards"][0]
        self.assertEqual(emitted["conditional_binding"], route)
        self.assertEqual(emitted["entry_kind"], "conditional")
        self.assertNotIn("routing_binding", emitted)
        self.assertNotIn("graph_path", emitted)
        self.assertEqual(result["selection"]["entry_omitted_count"], 0)
        self.assertNotIn("omitted", reader._selection_diagnostic(result, "turn", True))

    def test_malformed_missing_conflicting_binding_keeps_card_not_graph_feedback(self):
        from test_routing_memory import binding
        for changes in ({"conditional_binding": None}, {"conditional_binding": "x" * 2000},
                        {"graph_path": [{"forged": "hop"}]}, {"graph_path": {}},
                        {"routing_binding": binding()}):
            with self.subTest(changes=changes):
                result, runtime, _ = self.execute(changes)
                self.assertEqual(runtime.calls[0][1][0]["native"], {"entry_kind": "conditional"})
                self.assertEqual(len(result["cards"]), 1)
                self.assertNotIn("conditional_binding", result["cards"][0])
                self.assertNotIn("routing_binding", result["cards"][0])
        result, runtime, _ = self.execute({"entry_kind": None})
        self.assertNotIn("native", runtime.calls[0][1][0])
        self.assertNotIn("conditional_binding", result["cards"][0])

    def test_unknown_or_orphan_origin_cannot_salvage_graph_feedback(self):
        from test_routing_memory import binding
        route = binding()
        hop = {k: route["route"][k] for k in ("previous", "target", "from", "to")}
        for entry, orphan in (("future", False), (None, False), (None, True)):
            result, runtime, _ = self.execute({"entry_kind": entry, "graph_path": [hop],
                                              "routing_binding": route}, orphan=orphan)
            self.assertNotIn("native", runtime.calls[0][1][0])
            self.assertEqual(len(result["cards"]), 1)
            for field in ("entry_kind", "routing_binding", "conditional_binding"):
                self.assertNotIn(field, result["cards"][0])

    def test_selector_marker_omission_withholds_feedback_not_card(self):
        result, runtime, _ = self.execute(omit=True)
        self.assertNotIn("native", runtime.calls[0][1][0])
        self.assertEqual(result["selection"]["entry_omitted_count"], 1)
        self.assertEqual(result["selection"]["path_omitted_count"], 0)
        self.assertEqual(result["cards"][0]["entry_kind"], "conditional")
        self.assertNotIn("conditional_binding", result["cards"][0])

    def test_state_pressure_sheds_conditional_authority_before_card_or_usage(self):
        import copy
        from test_routing_memory import binding, TARGET, DB
        card = {**CARD_A, "id": TARGET, "entry_kind": "conditional", "conditional_binding": binding()}
        data = {"ready": {"cards": [card], "db_id": DB}, "input_tokens": 13, "output_tokens": 3}
        core = copy.deepcopy(data)
        core["ready"]["cards"][0].pop("conditional_binding")
        core["ready"]["cards"][0].pop("entry_kind")
        size = len(json.dumps(core, ensure_ascii=False, separators=(",", ":")).encode())
        with tempfile.TemporaryDirectory() as tmp, patch.object(reader, "MAX_STATE", size):
            path = Path(tmp) / "state.json"
            reader._write(path, data)
            self.assertEqual(json.loads(path.read_text()), core)


class MiscReaderWorkerTests(unittest.TestCase):
    """Disposable device config plus an ephemeral, non-enrolled workspace."""
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.workspace = self.root / "workspace"
        self.workspace.mkdir()
        self.service = self.root / "service.json"
        self.service.write_text(json.dumps({"mode": "connect", "url": "http://127.0.0.1:18766/",
            "database_name": "project", "database_path": "/srv/mneme/misc.db"}))
        binary = self.root / "reader"
        binary.write_text("#!/bin/sh\nexit 0\n")
        binary.chmod(0o700)
        import hashlib
        self.static = {"schema": "mneme.codex-hooks.config.v11", "memory_scope": "misc",
            "state_dir": str(self.root / "state"), "service_config": str(self.service),
            "memory_mode": "async", "recording_mode": "automatic", "reader_model": "gpt-6.1-sol",
            "librarian_effort": "medium", "reader_codex": str(binary),
            "reader_codex_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
            "store_target": {"db_alias": "project", "database_path": "/srv/mneme/misc.db", "db_id": PROJECT_DB},
            "excluded_roots": []}
        self.hook = self.root / "hooks.json"
        self.hook.write_text(json.dumps(self.static))
        self.binding = {"workspace_root": str(self.workspace), "workspace_origin": str(self.workspace)}
        from hooks import _config
        self.config = _config(self.hook, workspace_binding=self.binding)

    def test_detached_command_carries_binding_and_child_reload_keeps_origin(self):
        event = {"session_id": "s1", "turn_id": "t1", "prompt": "Investigate this mechanism"}
        self.assertEqual(reader.notice(self.config, event, True)["outcome"], "queued")
        with patch("reader_worker.subprocess.Popen") as spawn:
            spawn.return_value.pid = 12345
            self.assertEqual(reader.background(self.config, event)["outcome"], "launched")
        command = spawn.call_args.args[0]
        self.assertEqual(json.loads(command[command.index("--workspace-binding") + 1]), self.binding)
        with patch.object(reader, "serve") as serve:
            self.assertEqual(reader.main(command[2:]), 0)
        loaded = serve.call_args.args[0]
        self.assertEqual(loaded["workspace_binding"], self.binding)
        self.assertEqual(loaded["project_root"], self.workspace)
        self.assertNotIn("project_root", json.loads(self.hook.read_text()))

    def test_detached_reload_keeps_lexical_origin_and_canonical_root_distinct(self):
        alias = self.root / "workspace-alias"
        alias.symlink_to(self.workspace, target_is_directory=True)
        binding = {"workspace_root": str(self.workspace), "workspace_origin": str(alias)}
        with patch.object(reader, "serve") as serve:
            self.assertEqual(reader.main(["--serve", "--config", str(self.hook), "--session-id", "s1",
                "--workspace-binding", json.dumps(binding)]), 0)
        loaded = serve.call_args.args[0]
        self.assertEqual(loaded["workspace_binding"], binding)
        self.assertEqual(loaded["project_root"], self.workspace)

    def test_root_rebind_refuses_state_and_preserves_paid_identity(self):
        reader.notice(self.config, {"session_id": "s1", "turn_id": "t1", "prompt": "Investigate"}, True)
        reader._state(self.config, "s1", lambda d: (d.update(attempts=3, input_tokens=21,
            output_tokens=4, reservation={"paid": "original workspace"}), True))
        path, _ = reader._paths(self.config, "s1")
        before = path.read_bytes()
        other = self.root / "other"
        other.mkdir()
        from hooks import _config
        rebound = _config(self.hook, workspace_binding={"workspace_root": str(other), "workspace_origin": str(other)})
        self.assertFalse(reader._state(rebound, "s1", lambda d: (d.update(attempts=0), True))[0])
        self.assertFalse(reader._state(rebound, "s1", lambda d: (None, True), allow_stale=True)[0])
        self.assertEqual(path.read_bytes(), before)

    def test_same_origin_config_drift_never_refunds_paid_usage(self):
        reader.notice(self.config, {"session_id": "s1", "turn_id": "t1", "prompt": "Investigate"}, True)
        reader._state(self.config, "s1", lambda d: (d.update(attempts=3, input_tokens=21,
            output_tokens=4, reservation={"paid": "original workspace"}), True))
        self.static["librarian_effort"] = "high"
        self.hook.write_text(json.dumps(self.static))
        from hooks import _config
        current = _config(self.hook, workspace_binding=self.binding)
        self.assertTrue(reader._state(current, "s1", lambda d: (None, False))[0])
        data = json.loads(reader._paths(current, "s1")[0].read_text())
        self.assertEqual((data["attempts"], data["input_tokens"], data["output_tokens"]), (3, 21, 4))
        self.assertEqual(data["reservation"], {"paid": "original workspace"})
        self.assertEqual(data["workspace_binding"], self.binding)
        self.assertIsNone(reader._config_current(self.config))

    def test_exclusion_config_drift_and_new_enrollment_block_provider(self):
        runtime = FakeRuntime(self.config, None)
        with patch("hook_recall.collect_reader", return_value={"outcome": "ok", "cards": [CARD_A]}) as read:
            self.static["excluded_roots"] = [str(self.workspace)]
            self.hook.write_text(json.dumps(self.static))
            result = reader._execute(self.config, "Investigate mechanism", runtime, lambda: True, set())
        read.assert_not_called()
        self.assertFalse(result["provider_attempt"])
        self.assertEqual(runtime.calls, [])
        self.hook.write_text(json.dumps({**self.static, "excluded_roots": []}))
        from hooks import _config
        current = _config(self.hook, workspace_binding=self.binding)
        def reserve():
            (self.workspace / ".mneme").mkdir()
            return True
        with patch("hook_recall.collect_reader", return_value={"outcome": "ok", "cards": [CARD_A]}):
            result = reader._execute(current, "Investigate mechanism", runtime, reserve, set())
        self.assertFalse(result["provider_attempt"])
        self.assertEqual(runtime.calls, [])

    def test_child_rejects_tampered_and_oversized_binding_before_work(self):
        arguments = ["--serve", "--config", str(self.hook), "--session-id", "s1", "--workspace-binding"]
        for value in ("null", "[]", '{"workspace_root":"/","workspace_origin":"/tmp"}',
                      "x" * (reader.MAX_WORKSPACE_BINDING + 1)):
            with self.subTest(value=value[:60]), patch.object(reader, "serve") as serve:
                reader.main(arguments + [value])
                serve.assert_not_called()
        with patch("hooks._config", return_value={"memory_scope": "project"}), patch.object(reader, "serve") as serve:
            self.assertEqual(reader.main(arguments + [json.dumps(self.binding)]), 2)
        serve.assert_not_called()


if __name__ == "__main__":
    unittest.main()

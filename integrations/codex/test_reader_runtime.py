"""No live provider calls: a tiny line-protocol app-server exercises transport."""

import copy
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
from reader_runtime import (ReaderRuntime, TransportError, assessment_diagnostic,
                            RUNTIME_DIAGNOSTIC_REASONS, VALIDATION_DIAGNOSTIC_REASONS)
from reader_contract import BASE_INSTRUCTIONS, OUTPUT_SCHEMA, PROMPT_PREFIX, prepare
import recording_contract


FAKE_SERVER = r'''#!/usr/bin/env python3
import json, os, sys, time
mode = os.environ.get("FAKE_MODE", "normal")
trace = os.environ["FAKE_TRACE"]
thread_no = 0
turn_no = 0
previous_events = []
def send(message):
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()
def record(method, params):
    with open(trace, "a") as output:
        output.write(json.dumps({"method": method, "thread": params.get("threadId"),
                                 "pid": os.getpid(), "params": params}) + "\n")
for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialized":
        continue
    params = msg.get("params", {})
    record(method, params)
    request_id = msg["id"]
    if mode == "timeout" and method == "initialize":
        time.sleep(5)
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "thread/start":
        thread_no += 1
        send({"id": request_id, "result": {"thread": {"id": "thread-" + str(thread_no),
               "ephemeral": True}, "model": "gpt-6.1-sol", "approvalPolicy": "never",
               "instructionSources": []}})
    elif method == "turn/start":
        turn_no += 1
        turn_id = "turn-" + str(turn_no)
        thread_id = params["threadId"]
        send({"id": request_id, "result": {"turn": {"id": turn_id}}})
        if mode == "tool":
            send({"method": "item/completed", "params": {"item": {"type": "commandExecution"}}})
        else:
            if mode == "frames":
                for _ in range(1030):
                    send({"method": "item/updated", "params": {"item": {"type": "reasoning"}}})
            if mode in ("medium_frame", "oversized_frame", "aggregate_bytes"):
                size = {"medium_frame": 70000, "oversized_frame": 524288,
                        "aggregate_bytes": 400000}[mode]
                for _ in range(3 if mode == "aggregate_bytes" else 1):
                    send({"method": "item/updated", "params": {"padding": "x" * size}})
            usage = {"inputTokens": 33000 if mode == "large_input" else 120,
                     "cachedInputTokens": 20, "cacheWriteInputTokens": 0,
                     "outputTokens": 15, "reasoningOutputTokens": 4, "totalTokens": 135}
            if mode == "no_cache_write":
                usage.pop("cacheWriteInputTokens")
            if mode != "no_usage":
                send({"method": "thread/tokenUsage/updated", "params": {"threadId": thread_id, "turnId": turn_id,
                      "tokenUsage": {"last": usage, "total": usage}}})
            answer = {"selected_ids": ["c2", "c1"] if mode == "reorder" else ["c1"], "concerns": []}
            if mode == "bad_id": answer = {"selected_ids": ["not-a-card"], "concerns": []}
            recording = params.get("outputSchema", {}).get("required") == ["proposal"]
            if recording:
                answer = {"proposal": {"kind": "episode", "summary": "User corrected the revision.",
                          "body": "The user specified R5.", "evidence_ids": ["e001"]}}
                if mode in ("abstain", "answer_at_cap", "answer_above_cap"):
                    answer = {"proposal": None}
                if mode == "bad_evidence_id": answer["proposal"]["evidence_ids"] = ["e999"]
            if params.get("outputSchema", {}).get("required") == ["judgments"]:
                packet = json.loads(params["input"][0]["text"])
                answer = {"judgments": [{"witness": w["id"], "applicability": "matched",
                    "current_refs": [packet["current"][0]["id"]], "correction_applicable": False} for w in packet["history"]]}
            if params.get("outputSchema", {}).get("required") == ["decisions"]:
                packet = json.loads(params["input"][0]["text"])
                answer = {"decisions": [{"id": t["id"], "disposition": "noop", "tags": t["tags"]}
                                         for t in packet["targets"]]}
            raw_answer = json.dumps(answer)
            if mode == "duplicate_raw": raw_answer = '{"proposal":null,"proposal":null}'
            if mode == "duplicate_nested":
                raw_answer = raw_answer.replace('"kind": "episode"', '"kind": "lesson", "kind": "episode"')
            if mode == "nonfinite_raw": raw_answer = '{"proposal":null,"unused":NaN}'
            if mode == "infinity_raw": raw_answer = '{"proposal":Infinity}'
            if mode == "answer_at_cap": raw_answer += " " * (6144 - len(raw_answer.encode()))
            if mode == "answer_above_cap": raw_answer += " " * (6145 - len(raw_answer.encode()))
            answer_event = {"method": "item/completed", "params": {"threadId": thread_id, "turnId": turn_id,
                  "item": {"type": "agentMessage",
                  "text": raw_answer}}}
            completed_event = {"method": "turn/completed", "params": {"threadId": thread_id,
                  "turn": {"id": turn_id, "status": "completed"}}}
            send(answer_event)
            if mode == "stale_roles":
                for event in previous_events:
                    send(event)
            send(completed_event)
            previous_events = [answer_event, {"method": "thread/tokenUsage/updated",
                "params": {"threadId": thread_id, "turnId": turn_id,
                "tokenUsage": {"last": usage, "total": usage}}}, completed_event]
'''


class ReaderRuntimeTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.codex = self.root / "codex"
        self.codex.write_text(FAKE_SERVER.replace("#!/usr/bin/env python3", "#!" + sys.executable, 1))
        self.codex.chmod(0o700)
        self.auth = self.root / "auth.json"
        self.auth.write_text("{}")
        self.trace = self.root / "trace"
        self.config = {"reader_codex": str(self.codex),
                       "reader_codex_sha256": hashlib.sha256(self.codex.read_bytes()).hexdigest(),
                       "reader_model": "gpt-6.1-sol", "librarian_effort":"medium", "reader_auth": str(self.auth)}
        self.dialogue = [{"role": "user", "text": "Please send Caper's revision A batch without yesterday's leftovers."}]
        self.cards = [{"id": "c1", "summary": "Clear queue before submitting revision A batches.",
                       "source": "fixture://c1", "fingerprint": "one"},
                      {"id": "c2", "summary": "The revision B lamp moved right.",
                       "source": "fixture://c2", "fingerprint": "two"}]
        self.env = patch.dict(os.environ, {"FAKE_TRACE": str(self.trace), "FAKE_MODE": "normal"})
        self.env.start()
        self.addCleanup(self.env.stop)

    def rows(self):
        return [json.loads(line) for line in self.trace.read_text().splitlines()]

    def observation(self, *, extra_text=None):
        def reference(ordinal):
            return {"ordinal": ordinal, "line": ordinal + 1, "ordinal_scope": "source_turn",
                    "byte_offset": ordinal * 128, "line_bytes": 128,
                    "raw_line_sha256": hashlib.sha256(str(ordinal).encode()).hexdigest()}
        evidence = [{"kind": "user_statement", "ref": reference(1),
                     "content": [{"content_index": 0, "text": "no, R5"}]}]
        if extra_text is not None:
            evidence.append({"kind": "assistant_assertion", "phase": "final_answer",
                             "ref": reference(2), "content": [{"content_index": 0,
                                                                "text": extra_text}]})
        return {"schema": "mneme.codex-turn-observation.v3", "status": "complete",
                "reason": "source_turn_complete", "evidence": evidence,
                "boundary": reference(len(evidence) + 1),
                "coverage": {"source_turn": "closed_verified", "memory_delivery": "not_recorded",
                             "prompt": "verified", "earlier_session_context": "omitted",
                             "other_host_inputs": "omitted", "private_reasoning": "excluded",
                             "sensitivity_review": "not_performed", "missing_results": 0,
                             "omissions": {},
                             "public_evidence": {"mode": "all", "observed_records": len(evidence),
                                                 "selected_records": len(evidence),
                                                 "omitted_records": 0}}}

    def assessor_notifications(self):
        usage = {"inputTokens": 120, "cachedInputTokens": 20, "cacheWriteInputTokens": 0,
                 "outputTokens": 15, "reasoningOutputTokens": 4, "totalTokens": 135}
        return [
            {"method": "item/completed", "params": {"threadId": "current-thread", "turnId": "current-turn",
             "item": {"type": "agentMessage", "text": '{"proposal":null}'}}},
            {"method": "thread/tokenUsage/updated", "params": {"threadId": "current-thread",
             "turnId": "current-turn", "tokenUsage": {"last": usage, "total": dict(usage)}}},
            {"method": "turn/completed", "params": {"threadId": "current-thread",
             "turn": {"id": "current-turn", "status": "completed"}}},
        ]

    def assess_notifications(self, events, *, buffered=False, reader=False):
        """Exercise actual select/assess/_turn framing without a process."""
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            def start(*_args, **_kwargs):
                runtime._thread_id = "current-thread"
                runtime.cwd = self.root
            def request(*_args, **_kwargs):
                if buffered:
                    runtime._pending.extend(copy.deepcopy(events))
                return {"turn": {"id": "current-turn"}}
            incoming = [] if buffered else copy.deepcopy(events)
            with (patch.object(runtime, "_start", side_effect=start),
                  patch.object(runtime, "_request", side_effect=request),
                  patch.object(runtime, "_next", side_effect=incoming + [TransportError("timeout")])):
                result = (runtime.select(self.dialogue, self.cards) if reader
                          else runtime.assess(self.observation(), []))
                halted = runtime._halted
        return result, halted

    def test_contract_adds_kind_and_preserves_frozen_evidence_fields(self):
        root = Path(__file__).resolve().parents[2]
        sys.path.insert(0, str(root / "tools"))
        import codex_reader as frozen
        # The source prototype intentionally replaces two-card selection with
        # byte-budgeted delivery and makes memory kind explicit. Historical
        # evaluation policy stays frozen; ordinary evidence fields stay shared.
        self.assertNotEqual(BASE_INSTRUCTIONS, frozen.BASE_INSTRUCTIONS)
        expected_schema = json.loads(json.dumps(frozen.OUTPUT_SCHEMA))
        self.assertEqual(expected_schema["properties"]["selected_ids"].pop("maxItems"), 2)
        self.assertEqual(OUTPUT_SCHEMA["properties"]["selected_ids"], expected_schema["properties"]["selected_ids"])
        self.assertIn("concerns", OUTPUT_SCHEMA["required"])
        from reader_contract import validate_answer
        self.assertEqual(validate_answer({"selected_ids": ["c1", "c2", "c3"], "concerns": []},
                                         {"ids": ["c1", "c2", "c3"]})["selected_ids"], ["c1", "c2", "c3"])
        self.assertEqual(PROMPT_PREFIX, frozen.PROMPT_PREFIX)
        prompt, ids = prepare(self.dialogue, self.cards)
        self.assertEqual(ids["ids"], ["c1", "c2"])
        self.assertEqual(prompt, frozen.PROMPT_PREFIX + frozen._encode({
            "dialogue": frozen._window(self.dialogue),
            "cards": [{**card, "kind": "semantic"} for card in frozen._cards(self.cards)]
        }).decode())

    def test_route_invalid_config_refuses_before_unused_answer_bound(self):
        from test_routing_contract import CURRENT, DB, witness
        for changes in ({"librarian_effort":None},{"librarian_effort":"xhigh"},{"reader_model":None}):
            with self.subTest(changes=changes), ReaderRuntime({**self.config,**changes},self.root/"scratch") as runtime:
                result = runtime.route(CURRENT,[witness()],expected_db_id=DB)
                self.assertEqual(result["reason"],"invalid_config")
                self.assertFalse(result["provider_attempt"])
                self.assertIsNone(result["usage"])
                self.assertIsNone(runtime.process)

    def test_route_uses_context_schema_and_handles_more_than_eight(self):
        import routing_contract
        from test_routing_contract import RoutingWindowTests,DB
        current = [{"source":"task","text":"Inspect scope"}]
        rows = RoutingWindowTests.tiny_history(35)
        prompt,ctx = routing_contract.prepare(current,rows,expected_db_id=DB)
        with ReaderRuntime(self.config,self.root/"scratch") as runtime:
            result = runtime.route(current,rows,expected_db_id=DB)
        self.assertEqual(result["reason"],"matched")
        self.assertEqual(len(result["routing"].hints),1)
        self.assertEqual(len(result["routing"].decisions[0]["witnesses"]),35)
        turn = next(row["params"] for row in self.rows() if row["method"]=="turn/start")
        self.assertEqual(turn["outputSchema"],routing_contract.schema(ctx))
        self.assertEqual(turn["input"],[{"type":"text","text":prompt}])

    def test_reuse_rotation_and_usage(self):
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            receipts = [runtime.select(self.dialogue, self.cards) for _ in range(9)]
            self.assertEqual([r["reason"] for r in receipts], ["selected"] * 9)
            self.assertEqual(receipts[0]["usage"]["uncached_input_tokens"], 100)
            self.assertEqual(receipts[0]["usage"]["reasoning_output_tokens"], 4)
            self.assertEqual(receipts[0]["selected_ids"], ["c1"])
            self.assertTrue(all(r["provider_attempt"] for r in receipts))
        rows = self.rows()
        self.assertEqual(len([r for r in rows if r["method"] == "initialize"]), 1)
        self.assertEqual(len([r for r in rows if r["method"] == "thread/start"]), 2)
        turns = [r for r in rows if r["method"] == "turn/start"]
        self.assertEqual([r["thread"] for r in turns], ["thread-1"] * 8 + ["thread-2"])
        self.assertFalse(list((self.root / "scratch").iterdir()))

    def test_all_roles_share_selected_model_effort_but_not_history_or_grammar_caps(self):
        from test_routing_contract import CURRENT, DB, witness
        from librarian_policy import resolve
        for effort in ('low','medium','high'):
            self.trace.unlink(missing_ok=True)
            config={**self.config,'librarian_effort':effort}
            with ReaderRuntime(config,self.root/('scratch-'+effort)) as runtime:
                self.assertEqual(runtime.budget,resolve(config))
                receipts=[runtime.select(self.dialogue,self.cards),
                          runtime.route(CURRENT,[witness()],expected_db_id=DB),
                          runtime.assess(self.observation(),[]),
                          runtime.select(self.dialogue,self.cards)]
            self.assertEqual([r['reason'] for r in receipts],['selected','matched','proposed','selected'])
            starts=[r['params'] for r in self.rows() if r['method']=='thread/start']
            turns=[r for r in self.rows() if r['method']=='turn/start']
            self.assertTrue(all(r['model']=='gpt-6.1-sol' for r in starts))
            self.assertTrue(all(r['params']['model']=='gpt-6.1-sol' and r['params']['effort']==effort for r in turns))
            self.assertEqual([r['thread'] for r in turns],['thread-1','thread-2','thread-3','thread-1'])
            self.assertTrue(all(r['usage']['cached_input_tokens']==20 and r['usage']['reasoning_output_tokens']==4 for r in receipts))

    def test_missing_legacy_or_mutated_external_config_never_falls_back(self):
        for change in ({'reader_model':None},{'reader_model':'gpt-5.6-sol'},{'librarian_effort':None},{'librarian_effort':True}):
            config={**self.config,**change}
            with patch('reader_runtime.subprocess.Popen') as process,ReaderRuntime(config,self.root/'scratch') as runtime:
                for result in (runtime.select(self.dialogue,self.cards),runtime.assess(self.observation(),[])):
                    self.assertEqual(result['reason'],'invalid_config')
                    self.assertFalse(result['provider_attempt'])
                process.assert_not_called()
        config={**self.config,'librarian_effort':'low'}
        with ReaderRuntime(config,self.root/'scratch') as runtime:
            config['librarian_effort']='high';config['reader_model']='gpt-5.6-sol'
            self.assertEqual(runtime.select(self.dialogue,self.cards)['reason'],'selected')
            self.assertEqual(runtime.budget.effort,'low')
        turn=next(r['params'] for r in self.rows() if r['method']=='turn/start')
        self.assertEqual(turn['effort'],'low');self.assertEqual(turn['model'],'gpt-6.1-sol')

    def test_provider_reported_model_mismatch_refuses_without_turn_or_fallback(self):
        self.codex.write_text(self.codex.read_text().replace('"model": "gpt-6.1-sol"','"model": "gpt-5.6-sol"'))
        self.config['reader_codex_sha256']=hashlib.sha256(self.codex.read_bytes()).hexdigest()
        with ReaderRuntime(self.config,self.root/'scratch') as runtime:
            result=runtime.select(self.dialogue,self.cards)
        self.assertEqual(result['reason'],'thread_contract')
        self.assertFalse(any(r['method']=='turn/start' for r in self.rows()))

    def test_large_last_input_rotates_before_next_turn(self):
        os.environ["FAKE_MODE"] = "large_input"
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            self.assertEqual(runtime.select(self.dialogue, self.cards)["reason"], "selected")
            self.assertEqual(runtime.select(self.dialogue, self.cards)["reason"], "selected")
        self.assertEqual(len([r for r in self.rows() if r["method"] == "thread/start"]), 2)

    def test_unknown_usage_retires_and_halts(self):
        os.environ["FAKE_MODE"] = "no_usage"
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            first = runtime.select(self.dialogue, self.cards)
            second = runtime.select(self.dialogue, self.cards)
        self.assertEqual(first["reason"], "usage_unknown")
        self.assertEqual(first["selected_ids"], [])
        self.assertIsNone(first["usage"])
        self.assertTrue(first["provider_attempt"])
        self.assertEqual(second["reason"], "usage_unknown")
        self.assertFalse(second["provider_attempt"])
        self.assertEqual(len([r for r in self.rows() if r["method"] == "turn/start"]), 1)
        self.assertFalse(list((self.root / "scratch").iterdir()))

    def test_rejects_tool_activity_and_unknown_ids_without_fallback(self):
        for mode, expected in (("tool", "tool_activity"), ("bad_id", "invalid_output")):
            with self.subTest(mode=mode):
                os.environ["FAKE_MODE"] = mode
                with ReaderRuntime(self.config, self.root / ("scratch-" + mode)) as runtime:
                    result = runtime.select(self.dialogue, self.cards)
                self.assertEqual(result["reason"], expected)
                self.assertEqual(result["selected_ids"], [])
                if mode == "bad_id":
                    self.assertEqual(result["usage"]["input_tokens"], 120)
                else:
                    self.assertIsNone(result["usage"])

    def test_selected_ids_are_original_card_order(self):
        os.environ["FAKE_MODE"] = "reorder"
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            result = runtime.select(self.dialogue, self.cards)
        self.assertEqual(result["selected_ids"], ["c1", "c2"])

    def test_deadline_includes_startup_and_kills_process(self):
        os.environ["FAKE_MODE"] = "timeout"
        with patch("reader_runtime.TURN_SECONDS", 0.1):
            with ReaderRuntime(self.config, self.root / "scratch") as runtime:
                result = runtime.select(self.dialogue, self.cards)
        self.assertEqual(result["reason"], "timeout")
        self.assertLess(result["elapsed_ms"], 2000)
        self.assertFalse(list((self.root / "scratch").iterdir()))

    def test_invalid_input_never_starts_provider(self):
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            result = runtime.select(self.dialogue, [{"id": "c1", "summary": "x"}] * 2)
        self.assertEqual(result["reason"], "invalid_input")
        self.assertFalse(result["provider_attempt"])
        self.assertFalse(self.trace.exists())

    def test_preflight_miss_does_not_retire_existing_thread(self):
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            self.assertEqual(runtime.select(self.dialogue, self.cards)["reason"], "selected")
            miss = runtime.select([{"role": "user", "text": "thanks"}], self.cards)
            self.assertEqual(miss["reason"], "nonsubstantive")
            self.assertFalse(miss["provider_attempt"])
            self.assertEqual(runtime.select(self.dialogue, self.cards)["reason"], "selected")
        self.assertEqual(len([r for r in self.rows() if r["method"] == "initialize"]), 1)

    def test_frame_cap_retires_thread(self):
        os.environ["FAKE_MODE"] = "frames"
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            result = runtime.select(self.dialogue, self.cards)
        self.assertEqual(result["reason"], "frame_cap")
        self.assertEqual(result["selected_ids"], [])
        self.assertFalse(list((self.root / "scratch").iterdir()))

    def test_assess_proposes_bound_immutable_evidence(self):
        observation = self.observation()
        cards = [{"id": "native-hidden", "summary": "A potentially related note.",
                  "kind": "semantic"}]
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            result = runtime.assess(observation, cards, timeout=2)
            self.assertIsNotNone(runtime.process)
            self.assertIsNone(runtime._thread_id)
        self.assertEqual(result["reason"], "proposed")
        self.assertIsInstance(result["proposal"], recording_contract.Proposal)
        citation = result["proposal"].evidence[0]
        self.assertEqual(citation.evidence_id, "e001")
        self.assertEqual(citation.kind, "user_statement")
        self.assertFalse(hasattr(citation, "quote"))
        self.assertEqual(json.loads(citation.source_ref_json), observation["evidence"][0]["ref"])
        self.assertEqual(result["usage"]["uncached_input_tokens"], 100)
        self.assertTrue(result["provider_attempt"])
        self.assertFalse(list((self.root / "scratch").iterdir()))
        rows = self.rows()
        started = next(row["params"] for row in rows if row["method"] == "thread/start")
        self.assertEqual(started["baseInstructions"], recording_contract.BASE_INSTRUCTIONS)
        self.assertEqual(started["serviceName"], "mneme_recorder")
        self.assertIs(started["ephemeral"], True)
        self.assertEqual(started["approvalPolicy"], "never")
        self.assertEqual(started["sandbox"], "read-only")
        turn = next(row["params"] for row in rows if row["method"] == "turn/start")
        self.assertEqual(turn["outputSchema"], recording_contract.OUTPUT_SCHEMA)
        self.assertEqual(turn["sandboxPolicy"], {"type": "readOnly"})
        prompt, _ = recording_contract.prepare(observation, cards)
        self.assertEqual(turn["input"], [{"type": "text", "text": prompt}])
        self.assertNotIn("native-hidden", prompt)

    def test_steward_is_fresh_tool_free_thread_and_restores_selector(self):
        from fixture_stewardship import target, context
        import stewardship_contract
        with ReaderRuntime(self.config,self.root/"scratch") as runtime:
            first=runtime.select(self.dialogue,self.cards)
            result=runtime.steward([target()],context())
            last=runtime.select(self.dialogue,self.cards)
        self.assertEqual([first["reason"],result["reason"],last["reason"]],["selected","classified","selected"])
        self.assertEqual(result["decisions"][0]["disposition"],"noop")
        starts=[r["params"] for r in self.rows() if r["method"]=="thread/start"]
        self.assertEqual(len(starts),2)
        self.assertEqual(starts[1]["serviceName"],"mneme_tag_steward")
        self.assertEqual(starts[1]["baseInstructions"],stewardship_contract.INSTRUCTIONS)
        self.assertEqual([r["thread"] for r in self.rows() if r["method"]=="turn/start"],
                         ["thread-1","thread-2","thread-1"])
        self.assertEqual(starts[1]["approvalPolicy"],"never")
        self.assertEqual(starts[1].get("dynamicTools",[]),[])

    def test_steward_unknown_usage_halts_without_retry(self):
        from fixture_stewardship import target, context
        with patch.dict(os.environ,{"FAKE_MODE":"no_usage"}):
            with ReaderRuntime(self.config,self.root/"scratch") as runtime:
                first=runtime.steward([target()],context())
                second=runtime.steward([target()],context())
        self.assertEqual(first["reason"],"usage_unknown")
        self.assertTrue(first["provider_attempt"])
        self.assertIsNone(first["usage"])
        self.assertFalse(second["provider_attempt"])
        self.assertEqual(len([r for r in self.rows() if r["method"]=="turn/start"]),1)

    def test_matcher_thread_cannot_be_reused_as_selector_history(self):
        from test_routing_contract import CURRENT, witness, DB
        import routing_contract
        with ReaderRuntime(self.config, self.root / 'scratch') as runtime:
            first = runtime.select(self.dialogue, self.cards)
            matched = runtime.route(CURRENT, [witness()], expected_db_id=DB)
            last = runtime.select(self.dialogue, self.cards)
        self.assertEqual([first['reason'], matched['reason'], last['reason']], ['selected','matched','selected'])
        self.assertEqual(len(matched['routing'].hints), 1)
        starts = [row['params'] for row in self.rows() if row['method'] == 'thread/start']
        self.assertEqual(len(starts), 2)
        self.assertEqual(starts[1]['serviceName'], 'mneme_router')
        self.assertEqual(starts[1]['baseInstructions'], routing_contract.INSTRUCTIONS)
        self.assertEqual([row['thread'] for row in self.rows() if row['method']=='turn/start'],
                         ['thread-1', 'thread-2', 'thread-1'])
        self.assertEqual(len({row['pid'] for row in self.rows()}), 1)

    def test_missing_cache_write_is_unknown_across_route_select_and_assess(self):
        from test_routing_contract import CURRENT, witness, DB
        os.environ["FAKE_MODE"] = "no_cache_write"
        with ReaderRuntime(self.config, self.root / 'scratch') as runtime:
            receipts = [runtime.route(CURRENT, [witness()], expected_db_id=DB),
                        runtime.select(self.dialogue, self.cards),
                        runtime.assess(self.observation(), [])]
            self.assertFalse(runtime._halted)
        self.assertEqual([r['reason'] for r in receipts], ['matched', 'selected', 'proposed'])
        for receipt in receipts:
            self.assertIsNone(receipt['usage']['cache_write_input_tokens'])
            self.assertEqual(receipt['usage']['input_tokens'], 120)
            self.assertEqual(receipt['usage']['cached_input_tokens'], 20)
            self.assertEqual(receipt['usage']['uncached_input_tokens'], 100)
            self.assertEqual(receipt['usage']['output_tokens'], 15)
            self.assertEqual(receipt['usage']['total_tokens'], 135)

    def test_invalid_answer_retains_known_usage_with_unknown_cache_write(self):
        events = self.assessor_notifications()
        events[0]['params']['item']['text'] = '{"proposal": "invalid"}'
        events[1]['params']['tokenUsage']['last'].pop('cacheWriteInputTokens')
        # Unknown last, known cumulative cache-write is not evidence for this turn.
        receipt, halted = self.assess_notifications(events)
        self.assertEqual(receipt['reason'], 'invalid_output')
        self.assertIsNone(receipt['usage']['cache_write_input_tokens'])
        self.assertEqual(receipt['usage']['input_tokens'], 120)
        self.assertFalse(halted)

    def test_empty_match_preflight_preserves_existing_selector_thread(self):
        from test_routing_contract import CURRENT, DB
        with ReaderRuntime(self.config, self.root / 'scratch') as runtime:
            runtime.select(self.dialogue, self.cards)
            empty = runtime.route(CURRENT, [], expected_db_id=DB)
            runtime.select(self.dialogue, self.cards)
        self.assertFalse(empty['provider_attempt'])
        self.assertEqual(empty['reason'], 'no_history')
        self.assertEqual(len([row for row in self.rows() if row['method']=='thread/start']), 1)

    def test_assess_abstention_is_terminal_without_retry(self):
        os.environ["FAKE_MODE"] = "abstain"
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            result = runtime.assess(self.observation(), [])
        self.assertEqual(result["reason"], "abstained")
        self.assertIsNone(result["proposal"])
        self.assertIsNotNone(result["usage"])
        self.assertEqual(len([row for row in self.rows() if row["method"] == "turn/start"]), 1)

    def test_routed_assessment_changes_only_the_existing_close_call_contract(self):
        from test_recording_contract import observation, statement, memory_marker
        from test_routing_memory import binding, DB, TARGET
        from test_turn_observer import refresh_delivery_packet
        marker = memory_marker()
        marker['packet']['displayed'][0].update(db_id=DB, node_id=TARGET, routing_binding=binding())
        refresh_delivery_packet(marker['packet'])
        source = observation([statement('A changed constraint.'), marker,
                              statement('The old recommendation no longer fits.', ordinal=3)])
        prompt, context = recording_contract.prepare(source)
        with ReaderRuntime(self.config, self.root / 'scratch') as runtime:
            result = runtime.assess(source, [])
        self.assertEqual(result['reason'], 'proposed')  # Fake server returns an ordinary episode.
        rows = self.rows()
        turns = [row['params'] for row in rows if row['method'] == 'turn/start']
        self.assertEqual(len(turns), 1)
        self.assertEqual(turns[0]['outputSchema'], recording_contract.schema_for(context))
        start = next(row['params'] for row in rows if row['method'] == 'thread/start')
        self.assertEqual(start['baseInstructions'], recording_contract.instructions_for(context))
        self.assertIn('route_bound', prompt)
        self.assertNotIn('edge_fingerprint', prompt)

    def test_assess_uses_fresh_threads_and_restores_reader_contract(self):
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            receipts = [runtime.select(self.dialogue, self.cards),
                        runtime.assess(self.observation(), []),
                        runtime.assess(self.observation(), []),
                        runtime.select(self.dialogue, self.cards),
                        runtime.select(self.dialogue, self.cards)]
        self.assertEqual([r["reason"] for r in receipts],
                         ["selected", "proposed", "proposed", "selected", "selected"])
        rows = self.rows()
        self.assertEqual(len({row["pid"] for row in rows}), 1)
        self.assertEqual(len([row for row in rows if row["method"] == "initialize"]), 1)
        starts = [row["params"] for row in rows if row["method"] == "thread/start"]
        self.assertEqual([row["baseInstructions"] for row in starts],
                         [BASE_INSTRUCTIONS, recording_contract.BASE_INSTRUCTIONS,
                          recording_contract.BASE_INSTRUCTIONS])
        turns = [row for row in rows if row["method"] == "turn/start"]
        self.assertEqual([row["thread"] for row in turns],
                         ["thread-1", "thread-2", "thread-3", "thread-1", "thread-1"])
        self.assertEqual([row["params"]["outputSchema"] for row in turns],
                         [OUTPUT_SCHEMA, recording_contract.OUTPUT_SCHEMA,
                         recording_contract.OUTPUT_SCHEMA, OUTPUT_SCHEMA, OUTPUT_SCHEMA])

    def test_parked_selector_counters_usage_and_rotation_survive_both_roles(self):
        from test_routing_contract import CURRENT, witness, DB
        import reader_runtime as module
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            runtime.select(self.dialogue, self.cards)
            original = (runtime._thread_id, runtime._turns, runtime._last_input)
            runtime._turns = module.ROTATE_TURNS
            runtime._last_input = 12345
            for role in (lambda: runtime.route(CURRENT, [witness()], expected_db_id=DB),
                         lambda: runtime.assess(self.observation(), [])):
                result = role()
                self.assertIsNotNone(result["usage"])
                self.assertEqual((runtime._thread_id, runtime._turns, runtime._last_input),
                                 (original[0], module.ROTATE_TURNS, 12345))
                self.assertEqual(runtime._current_usage, result["usage"])
                self.assertEqual(runtime._pending, [])
            runtime.select(self.dialogue, self.cards)
            self.assertNotEqual(runtime._thread_id, original[0])
            runtime._last_input = module.ROTATE_INPUT_TOKENS
            parked = runtime._thread_id
            runtime.assess(self.observation(), [])
            self.assertEqual(runtime._last_input, module.ROTATE_INPUT_TOKENS)
            runtime.select(self.dialogue, self.cards)
            self.assertNotEqual(runtime._thread_id, parked)

    def test_parked_selector_is_discarded_on_role_failure_known_or_unknown_usage(self):
        for known in (True, False):
            with self.subTest(known_usage=known), ReaderRuntime(self.config, self.root / "scratch") as runtime:
                initial = runtime.select(self.dialogue, self.cards)
                process = runtime.process
                def fail(*args, **kwargs):
                    runtime._current_usage = initial["usage"] if known else None
                    raise TransportError("invalid_output" if known else "usage_unknown")
                with patch.object(runtime, "_turn", side_effect=fail):
                    failed = runtime.assess(self.observation(), [])
                self.assertIsNone(runtime._thread_id)
                self.assertIsNone(runtime.process)
                self.assertIsNotNone(process.poll())
                self.assertEqual(failed["usage"], initial["usage"] if known else None)
                later = runtime.select(self.dialogue, self.cards)
                self.assertEqual(later["reason"], "selected" if known else "usage_unknown")
                self.assertEqual(later["provider_attempt"], known)

    def test_reused_process_drops_late_events_in_both_role_directions(self):
        os.environ["FAKE_MODE"] = "stale_roles"
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            receipts = [runtime.select(self.dialogue, self.cards),
                        runtime.assess(self.observation(), []),
                        runtime.select(self.dialogue, self.cards),
                        runtime.assess(self.observation(), [])]
        self.assertEqual([r["reason"] for r in receipts], ["selected", "proposed", "selected", "proposed"])
        self.assertEqual(len({row["pid"] for row in self.rows()}), 1)
        self.assertTrue(all(r["usage"]["input_tokens"] == 120 for r in receipts))

    def test_assess_preflight_refusal_preserves_reader_thread(self):
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            self.assertEqual(runtime.select(self.dialogue, self.cards)["reason"], "selected")
            incomplete = self.observation()
            incomplete["status"] = "deferred"
            refused = runtime.assess(incomplete, [])
            self.assertEqual(refused["reason"], "source_turn_incomplete")
            self.assertFalse(refused["provider_attempt"])
            self.assertIsNone(refused["usage"])
            inconsistent = self.observation()
            inconsistent["coverage"]["public_evidence"]["mode"] = "selected_suffix"
            refused = runtime.assess(inconsistent, [])
            self.assertEqual(refused["reason"], "unsupported_public_selection")
            self.assertFalse(refused["provider_attempt"])
            self.assertEqual(runtime.select(self.dialogue, self.cards)["reason"], "selected")
        self.assertEqual(len([row for row in self.rows() if row["method"] == "thread/start"]), 1)

    def test_assess_invalid_timeouts_are_preflight(self):
        for timeout in (0, -1, 45.1, 10 ** 500, float("inf"), float("nan"), True, "1", None):
            with self.subTest(timeout=timeout):
                with ReaderRuntime(self.config, self.root / "scratch") as runtime:
                    result = runtime.assess(self.observation(), [], timeout=timeout)
                self.assertEqual(result["reason"], "invalid_timeout")
                self.assertFalse(result["provider_attempt"])
                self.assertIsNone(result["usage"])
        self.assertFalse(self.trace.exists())

    def test_assess_remaining_deadline_includes_startup(self):
        os.environ["FAKE_MODE"] = "timeout"
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            result = runtime.assess(self.observation(), [], timeout=0.1)
            blocked = runtime.select(self.dialogue, self.cards)
        self.assertEqual(result["reason"], "timeout")
        self.assertLess(result["elapsed_ms"], 2000)
        self.assertTrue(result["provider_attempt"])
        self.assertEqual(blocked["reason"], "usage_unknown")
        self.assertFalse(blocked["provider_attempt"])
        self.assertFalse(list((self.root / "scratch").iterdir()))

    def test_assess_unknown_usage_halts_both_roles(self):
        os.environ["FAKE_MODE"] = "no_usage"
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            first = runtime.assess(self.observation(), [])
            second = runtime.assess(self.observation(), [])
            reader = runtime.select(self.dialogue, self.cards)
        self.assertEqual([r["reason"] for r in (first, second, reader)], ["usage_unknown"] * 3)
        self.assertEqual([r["provider_attempt"] for r in (first, second, reader)], [True, False, False])
        self.assertIsNone(first["proposal"])
        self.assertIsNone(first["usage"])
        self.assertEqual(len([row for row in self.rows() if row["method"] == "turn/start"]), 1)

    def test_reader_unknown_usage_also_halts_assessment(self):
        os.environ["FAKE_MODE"] = "no_usage"
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            runtime.select(self.dialogue, self.cards)
            result = runtime.assess(self.observation(), [])
        self.assertEqual(result["reason"], "usage_unknown")
        self.assertFalse(result["provider_attempt"])
        self.assertEqual(len([row for row in self.rows() if row["method"] == "turn/start"]), 1)

    def test_assess_strict_json_and_unknown_source_ids_never_repair(self):
        for mode in ("duplicate_raw", "duplicate_nested", "nonfinite_raw", "infinity_raw", "bad_evidence_id"):
            with self.subTest(mode=mode):
                os.environ["FAKE_MODE"] = mode
                before = len(self.rows()) if self.trace.exists() else 0
                with ReaderRuntime(self.config, self.root / ("scratch-" + mode)) as runtime:
                    result = runtime.assess(self.observation(), [])
                self.assertEqual(result["reason"], "invalid_output")
                self.assertIsNone(result["proposal"])
                self.assertEqual(result["usage"]["input_tokens"], 120)
                self.assertTrue(result["provider_attempt"])
                self.assertEqual(len([row for row in self.rows()[before:]
                                      if row["method"] == "turn/start"]), 1)

    def test_assess_raw_answer_byte_boundary(self):
        for mode, expected in (("answer_at_cap", "abstained"), ("answer_above_cap", "invalid_output")):
            with self.subTest(mode=mode):
                os.environ["FAKE_MODE"] = mode
                with ReaderRuntime(self.config, self.root / ("scratch-" + mode)) as runtime:
                    result = runtime.assess(self.observation(), [])
                self.assertEqual(result["reason"], expected)
                self.assertIsNotNone(result["usage"])

    def test_assess_accepts_escaped_large_input_beyond_reader_frame(self):
        # Two-byte UTF-8 characters become six-byte escapes on the wire. The
        # authored input is bounded by the contract, not the escaped envelope.
        source = self.observation(extra_text="\u0080" * 15000)
        prompt, context = recording_contract.prepare(source, [])
        self.assertIsInstance(prompt, str, context)
        self.assertLessEqual(context.authored_bytes, 65536)
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            result = runtime.assess(source, [])
        self.assertEqual(result["reason"], "proposed")
        params = next(row["params"] for row in self.rows() if row["method"] == "turn/start")
        encoded_bytes = len(json.dumps(params).encode())
        self.assertGreater(encoded_bytes, 65536)
        self.assertLess(encoded_bytes, 524288)

    def test_assess_large_frame_allowance_does_not_broaden_reader(self):
        os.environ["FAKE_MODE"] = "medium_frame"
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            recording = runtime.assess(self.observation(), [])
            reader = runtime.select(self.dialogue, self.cards)
        self.assertEqual(recording["reason"], "proposed")
        self.assertEqual(reader["reason"], "frame_cap")

    def test_assess_transport_caps_and_tool_denials(self):
        for mode, expected in (("oversized_frame", "frame_cap"), ("aggregate_bytes", "frame_cap"),
                               ("frames", "frame_cap"), ("tool", "tool_activity")):
            with self.subTest(mode=mode):
                os.environ["FAKE_MODE"] = mode
                with ReaderRuntime(self.config, self.root / ("scratch-" + mode)) as runtime:
                    result = runtime.assess(self.observation(), [])
                    subsequent = runtime.assess(self.observation(), [])
                self.assertEqual(result["reason"], expected)
                self.assertIsNone(result["proposal"])
                self.assertTrue(result["provider_attempt"])
                self.assertEqual(subsequent["reason"], "usage_unknown")
                self.assertFalse(subsequent["provider_attempt"])
                self.assertFalse(list((self.root / ("scratch-" + mode)).iterdir()))

    def test_assess_decoder_recursion_returns_bounded_failure(self):
        source = self.observation()
        prepared = recording_contract.prepare(source, [])
        with ReaderRuntime(self.config, self.root / "scratch") as runtime:
            # Decoder depth limits vary by Python version; exercise its failure
            # contract without relying on an implementation-specific threshold.
            with (patch("recording_contract.prepare", return_value=prepared),
                  patch("reader_runtime.json.loads", side_effect=RecursionError)):
                result = runtime.assess(source, [])
        self.assertEqual(result["reason"], "invalid_events")
        self.assertTrue(result["provider_attempt"])
        self.assertIsNone(result["usage"])

    def test_assess_closed_runtime_never_starts(self):
        runtime = ReaderRuntime(self.config, self.root / "scratch")
        runtime.close()
        result = runtime.assess(self.observation(), [])
        self.assertEqual(result["reason"], "closed")
        self.assertFalse(result["provider_attempt"])
        self.assertFalse(self.trace.exists())

    def test_assess_foreign_answer_cannot_replace_current_abstention(self):
        proposal = {"proposal": {"kind": "episode", "summary": "An old answer.",
                    "body": "The user specified R5.", "evidence_ids": ["e001"]}}
        for field in ("threadId", "turnId", "both"):
            for buffered in (False, True):
                with self.subTest(field=field, buffered=buffered):
                    answer, usage, completed = self.assessor_notifications()
                    foreign = copy.deepcopy(answer)
                    foreign["params"]["item"]["text"] = json.dumps(proposal)
                    for key in (("threadId", "turnId") if field == "both" else (field,)):
                        foreign["params"][key] = "old-" + key
                    result, halted = self.assess_notifications([answer, foreign, usage, completed], buffered=buffered)
                    self.assertEqual(result["reason"], "abstained")
                    self.assertIsNone(result["proposal"])
                    self.assertEqual(result["usage"]["input_tokens"], 120)
                    self.assertFalse(halted)

    def test_assess_foreign_answer_alone_is_not_an_answer(self):
        answer, usage, completed = self.assessor_notifications()
        answer["params"].update(threadId="old-thread", turnId="old-turn")
        result, halted = self.assess_notifications([answer, usage, completed])
        self.assertEqual(result["reason"], "invalid_output")
        self.assertIsNone(result["proposal"])
        self.assertEqual(result["usage"]["input_tokens"], 120)
        self.assertFalse(halted)  # Current completion still establishes known usage.

    def test_reader_foreign_assessor_or_selector_answer_cannot_replace_abstention(self):
        for old_answer in ({"proposal": None}, {"selected_ids": ["c1"]}):
            for field in ("threadId", "turnId"):
                with self.subTest(old_answer=old_answer, field=field):
                    answer, usage, completed = self.assessor_notifications()
                    answer["params"]["item"]["text"] = '{"selected_ids":[],"concerns":[]}'
                    foreign = copy.deepcopy(answer)
                    foreign["params"][field] = "old-" + field
                    foreign["params"]["item"]["text"] = json.dumps(old_answer)
                    result, halted = self.assess_notifications([answer, foreign, usage, completed], reader=True)
                    self.assertEqual(result["reason"], "abstained")
                    self.assertEqual(result["selected_ids"], [])
                    self.assertEqual(result["usage"]["input_tokens"], 120)
                    self.assertFalse(halted)

    def test_reader_foreign_usage_and_completion_do_not_establish_current_outcome(self):
        for index in (1, 2):
            for field in ("threadId", "turnId"):
                with self.subTest(event=index, field=field):
                    events = self.assessor_notifications()
                    events[0]["params"]["item"]["text"] = '{"selected_ids":[],"concerns":[]}'
                    params = events[index]["params"]
                    if index == 2 and field == "turnId":
                        params["turn"]["id"] = "old-turn"
                    else:
                        params[field] = "old-" + field
                    result, halted = self.assess_notifications(events, reader=True)
                    self.assertEqual(result["reason"], "usage_unknown" if index == 1 else "timeout")
                    self.assertEqual(result["selected_ids"], [])
                    self.assertIsNone(result["usage"])
                    self.assertTrue(halted)

    def test_reader_unattributed_answer_usage_or_completion_fails_closed(self):
        for index in range(3):
            for field in ("threadId", "turnId"):
                with self.subTest(event=index, field=field):
                    events = self.assessor_notifications()
                    events[0]["params"]["item"]["text"] = '{"selected_ids":[],"concerns":[]}'
                    params = events[index]["params"]
                    if index == 2 and field == "turnId":
                        params["turn"].pop("id")
                    else:
                        params.pop(field)
                    result, halted = self.assess_notifications(events, reader=True)
                    self.assertEqual(result["reason"], "invalid_events")
                    self.assertEqual(result["selected_ids"], [])
                    self.assertIsNone(result["usage"])
                    self.assertTrue(halted)

    def test_assess_foreign_usage_cannot_supply_or_replace_current_usage(self):
        for field in ("threadId", "turnId"):
            with self.subTest(field=field):
                answer, usage, completed = self.assessor_notifications()
                foreign = copy.deepcopy(usage)
                foreign["params"][field] = "old-" + field
                foreign["params"]["tokenUsage"]["last"]["inputTokens"] = 900
                foreign["params"]["tokenUsage"]["total"]["inputTokens"] = 900
                result, halted = self.assess_notifications([answer, usage, foreign, completed])
                self.assertEqual(result["reason"], "abstained")
                self.assertEqual(result["usage"]["input_tokens"], 120)
                self.assertFalse(halted)
                result, halted = self.assess_notifications([answer, foreign, completed])
                self.assertEqual(result["reason"], "usage_unknown")
                self.assertIsNone(result["usage"])
                self.assertTrue(halted)

    def test_assess_foreign_completion_cannot_complete_or_fail_current_turn(self):
        for field in ("threadId", "turn.id"):
            with self.subTest(field=field):
                answer, usage, completed = self.assessor_notifications()
                foreign = copy.deepcopy(completed)
                if field == "threadId":
                    foreign["params"][field] = "old-thread"
                else:
                    foreign["params"]["turn"]["id"] = "old-turn"
                foreign["params"]["turn"]["status"] = "failed"
                result, halted = self.assess_notifications([answer, usage, foreign, completed])
                self.assertEqual(result["reason"], "abstained")
                self.assertFalse(halted)
                result, halted = self.assess_notifications([answer, usage, foreign])
                self.assertEqual(result["reason"], "timeout")
                self.assertIsNone(result["usage"])
                self.assertTrue(halted)

    def test_assess_missing_or_malformed_event_identifiers_fail_closed(self):
        absent = object()
        for index in range(3):
            for field in ("threadId", "turnId"):
                for value in (absent, None, "", 7, True):
                    with self.subTest(event=index, field=field, value=value):
                        events = self.assessor_notifications()
                        params = events[index]["params"]
                        target = params["turn"] if index == 2 and field == "turnId" else params
                        key = "id" if target is not params else field
                        if value is absent:
                            target.pop(key)
                        else:
                            target[key] = value
                        result, halted = self.assess_notifications(events)
                        self.assertEqual(result["reason"], "invalid_events")
                        self.assertIsNone(result["proposal"])
                        self.assertIsNone(result["usage"])
                        self.assertTrue(halted)
        for index in range(3):
            with self.subTest(event=index, params=None):
                events = self.assessor_notifications()
                events[index]["params"] = None
                result, halted = self.assess_notifications(events)
                self.assertEqual(result["reason"], "invalid_events")
                self.assertTrue(halted)

    def test_assessment_diagnostics_separate_raw_json_from_validator_failures(self):
        valid = {"proposal": {"kind": "episode", "summary": "User selected R5.", "body": "A user decision.",
                 "evidence_ids": ["e001"]}}
        changed = lambda: copy.deepcopy(valid)
        cases = [(None, "answer_type"), (7, "answer_type"),
                 ("not JSON", "invalid_json"), ('{"proposal":null,"proposal":null}', "invalid_json"),
                 ('{"proposal":NaN}', "invalid_json"), ('{"proposal":Infinity}', "invalid_json"),
                 ('{"proposal":1e999}', "unknown"),  # Encoder failure has no declared host reason.
                 (" " * 6145, "answer_bytes_limit"), ("[]", "invalid_answer_shape"),
                 ('{"proposal":[]}', "invalid_proposal_shape"),
                 ('{"proposal":null,"validation_reason":"invalid_exact_quote"}', "invalid_answer_shape")]
        nested = None
        for _ in range(34):
            nested = [nested]
        cases.append((json.dumps({"proposal": nested}), "unsupported_json_depth"))
        for change, expected in (
                (lambda p: p["proposal"].update(kind="fact"), "invalid_proposal_shape"),
                (lambda p: p["proposal"].update(summary=""), "invalid_proposal_text"),
                (lambda p: p["proposal"].update(summary="\ud800"), "invalid_answer"),
                (lambda p: p["proposal"].update(evidence_ids=[]), "invalid_citation_count"),
                (lambda p: p["proposal"].update(evidence_ids="e001"), "invalid_citation_count"),
                (lambda p: p["proposal"].update(evidence_ids=["e001"] * 5), "invalid_citation_count"),
                (lambda p: p["proposal"].update(evidence_ids=[7]), "invalid_citation_shape"),
                (lambda p: p["proposal"].update(evidence_ids=[{"evidence_id": "e001", "quote": "no, R5"}]), "invalid_citation_shape"),
                (lambda p: p["proposal"].update(evidence_ids=["e999"]), "invalid_citation_reference"),
                (lambda p: p["proposal"].update(associate_with="overlap001"), "invalid_proposal_shape"),
                (lambda p: p["proposal"].update(kind="lesson", associate_with="overlap001"),
                 "invalid_association_reference"),
                (lambda p: p["proposal"].update(kind_override="tool_result"), "invalid_proposal_shape"),
                (lambda p: p["proposal"].update(quote="no, R5"), "invalid_proposal_shape"),
                (lambda p: p["proposal"].update(evidence=[{"evidence_id": "e001", "kind": "user_statement", "quote": "no, R5"}]), "invalid_proposal_shape")):
            value = changed()
            change(value)
            cases.append((json.dumps(value), expected))
        for raw, expected in cases:
            with self.subTest(expected=expected, raw=raw):
                events = self.assessor_notifications()
                events[0]["params"]["item"]["text"] = raw
                result, halted = self.assess_notifications(events)
                self.assertEqual(result["reason"], "invalid_output")
                self.assertEqual(result["validation_reason"], expected)
                self.assertEqual(result["usage"]["input_tokens"], 120)
                self.assertFalse(halted)
                self.assertEqual(assessment_diagnostic(result),
                                 {"runtime_reason": "invalid_output", "validation_reason": expected})
        events = self.assessor_notifications()
        result, halted = self.assess_notifications(events[1:])
        self.assertEqual(result["validation_reason"], "answer_missing")
        self.assertIsNotNone(result["usage"])
        self.assertFalse(halted)

    def test_reader_contract_offers_more_than_eight_under_exact_prompt_bytes(self):
        cards = [{**self.cards[0], "id":"c" + str(i)} for i in range(9)]
        prompt, context = prepare(self.dialogue, cards[:8])
        self.assertEqual(len(context["ids"] if isinstance(context, dict) else context), 8)
        self.assertLessEqual(len(prompt.encode()), 12 * 1024)
        prompt,context=prepare(self.dialogue,cards)
        self.assertEqual(len(context["ids"]),9)
        self.assertLessEqual(len(prompt.encode()),12*1024)

    def test_delivery_grounding_rejection_preserves_reason_usage_and_no_retry(self):
        with patch("recording_contract.validate_answer", side_effect=ValueError("delivery_source_evidence_missing")):
            result, halted = self.assess_notifications(self.assessor_notifications())
        self.assertEqual(result["reason"], "invalid_output")
        self.assertEqual(result["validation_reason"], "delivery_source_evidence_missing")
        self.assertEqual(result["usage"]["input_tokens"], 120)
        self.assertFalse(halted)
        self.assertEqual(assessment_diagnostic(result), {"runtime_reason":"invalid_output",
                          "validation_reason":"delivery_source_evidence_missing"})

    def test_startup_uses_private_home_and_disables_ambient_extensions(self):
        import reader_runtime
        original = reader_runtime.subprocess.Popen
        captured = []
        def spawn(argv, **kwargs):
            captured.append((argv, kwargs["env"]))
            return original(argv, **kwargs)
        with patch.dict(os.environ, {"HOME":"/ambient", "CODEX_API_KEY":"secret",
                                     "OPENAI_API_KEY":"secret", "XDG_CONFIG_HOME":"/ambient/config"}), \
             patch.object(reader_runtime.subprocess, "Popen", side_effect=spawn):
            with ReaderRuntime(self.config, self.root / "isolation-scratch") as runtime:
                result = runtime.select(self.dialogue, self.cards)
                self.assertEqual(result["reason"], "selected")
                private_home = str(runtime.home)
        self.assertEqual(len(captured), 1)
        argv, env = captured[0]
        self.assertEqual(env["HOME"], private_home)
        self.assertEqual(env["CODEX_HOME"], private_home)
        for key in ("CODEX_API_KEY", "OPENAI_API_KEY", "XDG_CONFIG_HOME"):
            self.assertNotIn(key, env)
        disabled = [argv[i+1] for i, item in enumerate(argv[:-1]) if item == "--disable"]
        for feature in ("hooks","memories","multi_agent","shell_tool","plugins","apps",
                        "browser_use","computer_use","skill_search"):
            self.assertIn(feature, disabled)

    def test_assessment_diagnostic_unexpected_exception_text_is_not_retained(self):
        for error in (ValueError("private model response marker"), ValueError("invalid_exact_quote", "private"),
                      TypeError("invalid_exact_quote"), ValueError({"private": "response"})):
            with self.subTest(error_type=type(error).__name__):
                with patch("recording_contract.validate_answer", side_effect=error):
                    result, halted = self.assess_notifications(self.assessor_notifications())
                self.assertEqual(result["reason"], "invalid_output")
                self.assertEqual(result["validation_reason"], "unknown")
                self.assertIsNotNone(result["usage"])
                self.assertFalse(halted)
                self.assertNotIn("private", json.dumps(result))

    def test_assess_repeated_valid_ids_select_one_host_record_without_passage_claim(self):
        events = self.assessor_notifications()
        events[0]["params"]["item"]["text"] = json.dumps({"proposal": {
            "kind": "episode", "summary": "A fallible note.",
            "body": "This claim is not certified by syntactic source selection.",
            "evidence_ids": ["e001", "e001"]}})
        result, halted = self.assess_notifications(events)
        self.assertEqual(result["reason"], "proposed")
        self.assertFalse(halted)
        self.assertEqual(result["usage"]["input_tokens"], 120)
        self.assertEqual(len(result["proposal"].evidence), 1)
        citation = result["proposal"].evidence[0]
        self.assertEqual(set(vars(citation)), {
            "evidence_id", "kind", "source_ref_json", "source_field", "rendering"})
        self.assertEqual((citation.evidence_id, citation.kind), ("e001", "user_statement"))
        self.assertEqual(json.loads(citation.source_ref_json), self.observation()["evidence"][0]["ref"])
        self.assertNotIn("quote", vars(citation))
        self.assertNotIn("text", vars(citation))

    def test_historical_quote_diagnostics_remain_readable_without_old_contract_fallback(self):
        for code in ("invalid_exact_quote", "duplicate_citation"):
            self.assertEqual(assessment_diagnostic({"reason": "invalid_output", "validation_reason": code}),
                             {"runtime_reason": "invalid_output", "validation_reason": code})
        self.assertNotIn('"quote"', json.dumps(recording_contract.OUTPUT_SCHEMA))
        self.assertNotIn('"evidence"', json.dumps(recording_contract.OUTPUT_SCHEMA))

    def test_assessment_diagnostic_projection_is_total_and_bounded(self):
        class Untrusted:
            def __str__(self):
                raise AssertionError("must not stringify diagnostic input")
            def __hash__(self):
                raise AssertionError("must not hash diagnostic input")
        class UntrustedDict(dict):
            def get(self, *args):
                raise AssertionError("must not call untrusted dictionary methods")
        unknown = {"runtime_reason": "unknown", "validation_reason": "unknown"}
        for value in (None, [], "private text", 7, Untrusted(), UntrustedDict(reason="invalid_output"), {}):
            self.assertEqual(assessment_diagnostic(value), unknown)
        for value in (Untrusted(), [], {}, True, "private text", "x" * 10000):
            self.assertEqual(assessment_diagnostic({"reason": value, "validation_reason": "invalid_exact_quote"}), unknown)
            self.assertEqual(assessment_diagnostic({"reason": "invalid_output", "validation_reason": value}),
                             {"runtime_reason": "invalid_output", "validation_reason": "unknown"})
        for reason in RUNTIME_DIAGNOSTIC_REASONS:
            for validation in VALIDATION_DIAGNOSTIC_REASONS:
                value = assessment_diagnostic({"reason": reason, "validation_reason": validation,
                                               "raw_answer": "private text", "exception": Untrusted()})
                self.assertEqual(set(value), {"runtime_reason", "validation_reason"})
                self.assertEqual(value["validation_reason"], validation if reason == "invalid_output" else "unknown")
                self.assertTrue(all(type(code) is str and len(code) <= 64 for code in value.values()))
                self.assertLessEqual(len(json.dumps(value).encode()), 256)
                self.assertNotIn("private", json.dumps(value))

    def test_assessment_diagnostics_do_not_change_null_or_usage_failures(self):
        result, halted = self.assess_notifications(self.assessor_notifications())
        self.assertEqual(result["reason"], "abstained")
        self.assertIsNone(result["proposal"])
        self.assertIsNotNone(result["usage"])
        self.assertFalse(halted)
        self.assertNotIn("validation_reason", result)
        self.assertEqual(assessment_diagnostic(result), {"runtime_reason": "abstained", "validation_reason": "unknown"})
        for mode in ("missing_usage", "timeout", "provider_error"):
            with self.subTest(mode=mode):
                events = self.assessor_notifications()
                if mode == "missing_usage":
                    events.pop(1)
                elif mode == "timeout":
                    events.pop()
                else:
                    events[-1]["params"]["turn"]["status"] = "failed"
                result, halted = self.assess_notifications(events)
                expected = "usage_unknown" if mode == "missing_usage" else mode
                self.assertEqual(result["reason"], expected)
                self.assertIsNone(result["usage"])
                self.assertTrue(halted)
                self.assertNotIn("validation_reason", result)
                self.assertEqual(assessment_diagnostic(result), {"runtime_reason": expected, "validation_reason": "unknown"})

    def test_reader_receipt_does_not_gain_assessment_diagnostics(self):
        result, _ = self.assess_notifications(self.assessor_notifications(), reader=True)
        self.assertEqual(result["reason"], "invalid_output")
        self.assertEqual(set(result), {"selected_ids", "concerns", "reason", "usage", "elapsed_ms", "provider_attempt"})


if __name__ == "__main__":
    unittest.main()


class AssessmentNormalizationTests(unittest.TestCase):
    def test_runtime_unpacks_independent_maintenance_and_preserves_usage(self):
        runtime = object.__new__(ReaderRuntime)
        marker = object()
        usage = {"input_tokens": 10, "output_tokens": 3}
        def fresh(*args, **kwargs):
            value = {"proposal": None, "maintenance": [marker]}
            self.assertEqual(kwargs["success"](value), "proposed")
            self.assertEqual(kwargs["success"]({"proposal": None, "maintenance": []}), "abstained")
            return {"assessment": value, "reason": "proposed", "usage": usage,
                    "provider_attempt": True, "elapsed_ms": 1}
        with patch.object(runtime, "_fresh_assessment", side_effect=fresh):
            result = runtime.assess({})
        self.assertIsNone(result["proposal"])
        self.assertEqual(result["maintenance"], [marker])
        self.assertIs(result["usage"], usage)
        self.assertNotIn("assessment", result)


    def test_partial_intent_omissions_are_not_false_abstention(self):
        runtime = object.__new__(ReaderRuntime)
        omissions = {"proposal": "invalid_proposal_shape", "maintenance": {"invalid_maintenance_reference": 2}}
        def fresh(*args, **kwargs):
            value = {"proposal": None, "maintenance": [], "omissions": omissions}
            reason = kwargs["success"](value)
            self.assertEqual(reason, "validation_failed")
            self.assertEqual(kwargs["success"]({**value, "maintenance": [object()]}), "proposed")
            return {"assessment": value, "reason": reason, "usage": {"input_tokens": 10, "output_tokens": 2},
                    "provider_attempt": True, "elapsed_ms": 1}
        with patch.object(runtime, "_fresh_assessment", side_effect=fresh):
            result = runtime.assess({})
        self.assertEqual(result["intent_omissions"], omissions)
        self.assertNotIn("omissions", result)
        self.assertEqual(result["usage"]["input_tokens"], 10)

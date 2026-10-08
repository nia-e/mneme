"""Synthetic durable recording lifecycle; no models, native stores or real transcripts."""
from __future__ import annotations

import copy
import fcntl
import hashlib
import json
from pathlib import Path
import sys
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import recording_contract as contract
import recording_jobs as jobs
import fixture_source_turn as source
from librarian_policy import resolve
from fixture_recording import DB_ID, OTHER_DB, PROMPT, SESSION, FakeRuntime, RecordingFixture

REAL_MONOTONIC = time.monotonic


class RecordingJobsTests(unittest.TestCase, RecordingFixture):
    def setUp(self):
        RecordingFixture.__init__(self, self)

    def test_possibility_is_one_ordinary_note_with_exact_category_tag(self):
        self.closed()
        self.step(FakeRuntime(self, kind="possibility"))
        self.assertEqual(len(self.writes), 1)
        frozen = self.writes[0][1]
        self.assertEqual(frozen["proposal_kind"], "possibility")
        self.assertEqual(frozen["payload"]["tags"], ["possibility"])
        self.assertNotIn("action", frozen["payload"])
        self.assertNotIn("links", frozen["payload"])
        self.assertNotIn("routing", frozen)
        self.assertNotIn("association", frozen)

    def test_sync_session_end_without_stop_detaches_or_retains_final_jobs(self):
        import hooks
        import reader_worker
        config = {**self.config, "memory_mode": "async", "project_root": self.root.resolve(),
                  "_config_path": self.root / "hook.json"}
        config["_config_path"].write_text("{}")
        self.config = config
        self.begin()
        reader_worker.notice(config, self.event(), False)
        event = {"hook_event_name": "SessionEnd", "cwd": str(self.root), "session_id": SESSION}
        with patch.object(hooks, "_reader_worker", return_value=reader_worker), \
             patch("reader_worker.subprocess.Popen", side_effect=OSError("fixture spawn failure")) as spawn, \
             patch("reader_worker.serve", side_effect=AssertionError("inline assessment")):
            self.assertEqual(hooks.handle_event(event, config), {})
        spawn.assert_called_once()
        self.assertTrue(spawn.call_args.kwargs["start_new_session"])
        self.assertEqual(spawn.call_args.kwargs["stdin"], reader_worker.subprocess.DEVNULL)
        self.assertEqual(spawn.call_args.kwargs["stdout"], reader_worker.subprocess.DEVNULL)
        self.assertEqual(spawn.call_args.kwargs["stderr"], reader_worker.subprocess.DEVNULL)
        self.assertTrue(self.state()["ended"])
        self.assertEqual(self.state()["jobs"][0]["phase"], "observing")
        self.assertTrue(jobs.has_work(config, SESSION))
        self.assertEqual(self.writes, [])
        # A later bounded wake can detach the same retained work, without a new
        # allowance or resurrecting reader cards after the session ended.
        with patch.object(hooks, "_reader_worker", return_value=reader_worker), \
             patch("reader_worker.subprocess.Popen") as spawn, patch("reader_worker._alive", return_value=False):
            spawn.return_value.pid = 12345
            self.assertEqual(hooks.handle_event(event, config), {})
        spawn.assert_called_once()
        reader_state = json.loads(reader_worker._paths(config, SESSION)[0].read_text())
        self.assertTrue(reader_state["ended"])
        self.assertIsNone(reader_state["ready"])
        self.assertEqual(reader_state["attempts"], 0)
        self.assertEqual(self.state()["jobs"][0]["phase"], "observing")

    def remove_context_diagnostics(self):
        # Keep legacy assessment/observation-pressure tests isolated to those fields.
        def remove(data):
            for row in data['jobs'] + data['receipts']:
                row.pop('context_diagnostic', None)
                row.pop('context_diagnostic_omitted', None)
        self.mutate(remove)

    def test_workshop_routing_keeps_exact_source_without_changing_ordinary_notes(self):
        import routing_memory
        self.root = self.root.resolve()
        self.service.write_text(json.dumps({"mode": "connect", "url": "http://127.0.0.1:18766/",
            "database_name": "user", "database_path": "/srv/mneme/workshop.db"}))
        self.config.update(schema="mneme.codex-hooks.config.v9", memory_scope="workshop",
            project_root=self.root, service_config=self.service.resolve(), memory_mode="async",
            store_target={"db_alias": "user", "database_path": "/srv/mneme/workshop.db", "db_id": DB_ID})
        owner = self
        class ScopedRuntime(FakeRuntime):
            def assess(self, observation, overlap, *, timeout, **scope_options):
                owner.assertEqual(scope_options, {"recording_scope": "workshop", "expected_db_id": DB_ID})
                return super().assess(observation, overlap, timeout=timeout)
        def write(config, job, timeout):
            self.assertEqual(self.state()["jobs"][0]["phase"], "write_intent")
            self.writes.append((config, dict(job), timeout))
            return {"db": "user", "db_id": DB_ID, "id": OTHER_DB, "readback_status": "verified"}
        for lane in ("ordinary", "routing", "conditional"):
            with self.subTest(lane=lane):
                if self.path().exists():
                    self.path().unlink()
                if jobs._path(self.config, SESSION).exists():
                    jobs._path(self.config, SESSION).unlink()
                if lane == "ordinary":
                    self.closed()
                else:
                    self.routed_closed(conditional=lane == "conditional")
                _, runtime = self.step(ScopedRuntime(self, routing=lane != "ordinary"), native_write=write)
                self.assertEqual(len(runtime.calls), 1)
                payload = self.writes[-1][1]["payload"]
                self.assertEqual(self.state()["receipts"][-1]["outcome"], "verified")
                if lane == "ordinary":
                    self.assertEqual(payload["source"]["reference"], f"codex://{SESSION}/turn-1?scope=workshop&db_id={DB_ID}")
                    self.assertEqual(payload["source"]["namespace"], "codex-acquisition.v1")
                    self.assertEqual(payload["body"], "The user corrected the target to R5.")
                else:
                    self.assertEqual(payload["source"]["reference"], f"codex://{SESSION}/turn-1")
                    body = payload["body"]
                    history = {"id": OTHER_DB, "db_id": DB_ID, "status": "active",
                        "memory_kind": {"kind": "semantic"}, "tags": payload["tags"], "body": body,
                        "provenance": {"type": "external", "source": payload["source"]},
                        "body_range": {"source_start": 0, "source_end": len(body.encode()),
                                       "has_more": False, "next_offset": None}}
                    decoded = routing_memory.decode_witness(history, expected_db_id=DB_ID)
                    self.assertIsNotNone(decoded)
                    self.assertEqual(decoded["witness"].get("entry_kind"), "conditional" if lane == "conditional" else None)
                    self.assertEqual(decoded["witness"]["note"], "The user explicitly selected R5 rather than the delivered R6 advice.")
                    self.assertEqual(self.state()["receipts"][-1]["routing"]["outcome"], "included")

    def test_native_save_keeps_frozen_jobs_and_guard(self):
        for proposal_kind in ("lesson", "episode", "routing", "possibility"):
            payload = {"summary": "A recorded fact", "source": {
                "namespace": "codex", "key": "stable-source", "reference": "codex://test"}}
            if proposal_kind == "episode":
                payload["action"] = "append"
            if proposal_kind == "possibility":
                payload["tags"] = ["possibility"]
            job = {"config_sha256": "binding", "db_id": DB_ID,
                   "proposal_kind": proposal_kind, "payload": payload}
            before = copy.deepcopy(job)
            with patch("service.load_config", return_value=SimpleNamespace(token_env="", url="http://127.0.0.1:12345/")), \
                    patch("recording_jobs._config_digest", return_value="binding"), \
                    patch("mcp_client.McpClient") as factory:
                client = factory.return_value
                client.save_verified.return_value = {"id": OTHER_DB, "readback_status": "verified"}
                self.assertEqual(jobs._native_write(self.config, job, 2)["id"], OTHER_DB)
                expected = {k: v for k, v in payload.items() if k != "action"}
                expected["kind"] = "episode" if proposal_kind == "episode" else "note"
                client.save_verified.assert_called_once_with("project", expected, expected_db_id=DB_ID)
                client.capture_verified.assert_not_called()
                client.episode_verified.assert_not_called()
                client.close.assert_called_once()
            self.assertEqual(job, before)

    def test_frozen_possibility_cannot_gain_tags_or_operations_on_replay(self):
        base = {"config_sha256": "binding", "db_id": DB_ID, "proposal_kind": "possibility",
                "payload": {"summary": "Open question.", "tags": ["possibility"]}}
        for change in ({"tags": ["possibility", "pursuing"]}, {"tags": []},
                       {"action": "append"}, {"links": [{"to": OTHER_DB}]}):
            job = copy.deepcopy(base)
            job["payload"].update(change)
            with self.subTest(change=change), \
                    patch("service.load_config", return_value=SimpleNamespace(token_env="", url="http://127.0.0.1:12345/")), \
                    patch("recording_jobs._config_digest", return_value="binding"), \
                    patch("mcp_client.McpClient") as factory:
                with self.assertRaisesRegex(ValueError, "possibility_operation"):
                    jobs._native_write(self.config, job, 2)
                factory.assert_not_called()

    def test_native_save_rejects_revision_and_does_not_fallback(self):
        from mcp_client import McpProtocolError
        job = {"config_sha256": "binding", "db_id": DB_ID, "proposal_kind": "episode",
               "payload": {"action": "revise", "summary": "Not an append"}}
        with patch("service.load_config", return_value=SimpleNamespace(token_env="", url="http://127.0.0.1:12345/")), \
                patch("recording_jobs._config_digest", return_value="binding"), \
                patch("mcp_client.McpClient") as factory:
            client = factory.return_value
            with self.assertRaisesRegex(ValueError, "requires_episode_append"):
                jobs._native_write(self.config, job, 2)
            client.save_verified.assert_not_called()
            client.close.assert_called_once()
            client.reset_mock()
            job["payload"]["action"] = "append"
            client.save_verified.side_effect = McpProtocolError("unsupported SAVE")
            with self.assertRaisesRegex(McpProtocolError, "unsupported SAVE"):
                jobs._native_write(self.config, job, 2)
            client.save_verified.assert_called_once()
            client.capture_verified.assert_not_called()
            client.episode_verified.assert_not_called()
            client.close.assert_called_once()

    def interrupt_with_queue_lock(self, turn="turn-1"):
        with (jobs._directory(self.config) / ".lock").open("r+") as stream:
            fcntl.flock(stream.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            started = REAL_MONOTONIC()
            jobs.close_turn(self.config, SESSION, turn, cancel=True)
            return {"elapsed": REAL_MONOTONIC() - started,
                    "marker": jobs._cancel_marker(self.config, SESSION),
                    "state": self.state()}

    def test_both_fresh_startup_and_source_start_orderings(self):
        for before in (True, False):
            session = "ordering-" + str(before)
            if before:
                self.startup(session)
            self.source_start(session=session)
            if not before:
                self.startup(session)
            with self.subTest(startup_before=before):
                self.assertEqual(jobs.notice(self.config, self.event(session=session))["outcome"], "admitted")
                self.assertEqual(self.state(session)["counts"]["admitted"], 1)
        self.identity.assert_not_called()
        self.overlap.assert_not_called()

    def test_duplicate_startup_prompt_and_changed_same_turn_cannot_mint_jobs(self):
        first = self.begin()
        before = self.state()
        self.startup()
        self.assertEqual(jobs.notice(self.config, self.event())["outcome"], "replay")
        changed = jobs.notice(self.config, self.event(prompt="Actually R6"))
        self.assertIn(changed["outcome"], ("identity_conflict", "deferred"))
        after = self.state()
        self.assertEqual(after["jobs"], before["jobs"])
        self.assertFalse(after["fresh"])
        self.assertEqual(after["jobs"][0]["key"], first["key"])

    def test_delivery_packet_is_optional_first_valid_admitted_turn_only(self):
        packet = self.delivery_packet()
        self.assertEqual(jobs.attach_delivery(self.config, packet)["outcome"], "busy_or_missing")
        self.begin()
        self.assertEqual(jobs.attach_delivery(self.config, packet)["outcome"], "attached")
        self.assertEqual(jobs.attach_delivery(self.config, self.delivery_packet(text="later"))["outcome"], "replay")
        self.assertEqual(self.state()["jobs"][0]["delivery"], packet)
        self.assertEqual(jobs.attach_delivery(self.config, self.delivery_packet(turn="wrong"))["outcome"],
                         "missing_or_closed")
        bad = {**packet, "rendered_sha256": "0" * 64}
        self.assertEqual(jobs.attach_delivery(self.config, bad)["outcome"], "invalid_or_unavailable")
        jobs.close_turn(self.config, SESSION, "turn-1", cancel=True)
        self.assertEqual(self.state()["jobs"], [])
        self.assertNotIn("delivery", self.state()["receipts"][-1])
        self.assertNotIn("Exact Mneme hook output", jobs.encoded(self.state()).decode())

    def test_delivery_packet_failure_does_not_change_existing_job(self):
        self.begin()
        original = copy.deepcopy(self.state()["jobs"][0])
        packet = self.delivery_packet()
        self.service.write_text('{"changed":true}')
        self.assertEqual(jobs.attach_delivery(self.config, packet)["outcome"], "stale")
        self.assertEqual(self.state()["jobs"][0], original)

    def test_optional_delivery_drops_before_mandatory_job_at_final_state_pressure(self):
        self.begin()
        self.assertEqual(jobs.attach_delivery(self.config, self.delivery_packet())["outcome"], "attached")
        core = dict(self.state()["jobs"][0])
        core.pop("delivery")
        padding = jobs.MAX_JOB_BYTES - len(jobs.encoded(core)) - 160
        self.assertGreater(padding, 0)
        self.mutate(lambda data: data["jobs"][0].update(padding="x" * padding))
        self.assertEqual(len(self.state()["jobs"]), 1)
        self.assertNotIn("delivery", self.state()["jobs"][0])

    def test_tagged_delivery_uses_same_one_shot_assessment_and_historical_target(self):
        self.begin()
        packet = self.delivery_packet()
        self.assertEqual(jobs.attach_delivery(self.config, packet)["outcome"], "attached")
        self.append([source.row("response_item", {
            "type": "message", "id": "hook-packet", "role": "developer",
            "content": [{"type": "input_text", "text": packet["rendered_text"]}],
            source.META: {"turn_id": "turn-1", "content_item_kinds": ["hooks.additional_context"]}})])
        self.source_complete()
        jobs.close_turn(self.config, SESSION, "turn-1")
        with patch("hook_recall.check_association_target", return_value="kept") as check:
            _, runtime = self.step(FakeRuntime(self, associate=True))
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(len(self.writes), 1)
        self.assertEqual(runtime.calls[0][0]["coverage"]["memory_delivery"], "admitted_retained")
        self.assertEqual(check.call_args.args[2].origin, "delivery")
        self.assertEqual(check.call_args.args[2].full_get_fingerprint, "a" * 64)
        self.assertEqual(self.writes[0][1]["payload"]["links"][0]["to"], OTHER_DB)
        receipt = self.state()["receipts"][-1]
        self.assertEqual(receipt["association"]["outcome"], "requested")
        self.assertNotIn("delivery", receipt)
        self.assertNotIn(packet["rendered_text"], jobs.encoded(receipt).decode())

    def routed_closed(self, *, conditional=False):
        from test_routing_memory import binding, PREVIOUS
        self.begin()
        packet = self.delivery_packet()
        route = binding()
        route['db_id'] = DB_ID
        route['route'].update(previous=PREVIOUS, target=OTHER_DB, **{'from': PREVIOUS, 'to': OTHER_DB})
        if conditional:
            packet['displayed'][0].update(entry_kind='conditional', conditional_binding=route)
        else:
            packet['displayed'][0]['routing_binding'] = route
        self.assertEqual(jobs.attach_delivery(self.config, packet)['outcome'], 'attached')
        self.append([source.delivered(packet, turn='turn-1')])
        self.source_complete()
        jobs.close_turn(self.config, SESSION, 'turn-1')
        return route

    def test_conditional_annotation_uses_existing_one_shot_assessment_and_capture(self):
        import routing_memory
        route = self.routed_closed(conditional=True)
        _, runtime = self.step(FakeRuntime(self, routing=True))
        self.assertEqual((len(runtime.calls), len(self.writes), len(self.reservations)), (1, 1, 1))
        self.assertTrue(self.overlap.call_args.kwargs['include_routing'])
        payload = self.writes[0][1]['payload']
        self.assertNotIn('links', payload)
        witness = routing_memory.validate_witness(json.loads(payload['body']), expected_db_id=DB_ID)
        self.assertEqual(witness['entry_kind'], 'conditional')
        self.assertEqual(witness['binding'], route)
        self.assertEqual(witness['source_turn'], {'session':SESSION, 'turn':'turn-1'})
        self.assertEqual({item['kind'] for item in witness['evidence']}, {'memory_delivery', 'user_statement'})
        self.assertEqual(self.state()['receipts'][-1]['routing']['outcome'], 'included')
        self.assertFalse(self.step(runtime)[0])

    def test_routing_annotation_uses_one_assessment_one_capture_and_no_replay(self):
        import routing_memory
        route = self.routed_closed()
        _, runtime = self.step(FakeRuntime(self, routing=True))
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(len(self.writes), 1)

        self.assertEqual(len(self.reservations), 1)
        self.assertTrue(self.overlap.call_args.kwargs['include_routing'])
        payload = self.writes[0][1]['payload']
        self.assertEqual(payload['source']['namespace'], routing_memory.NAMESPACE)
        self.assertEqual(payload['tags'], [routing_memory.TAG])
        self.assertNotIn('links', payload)  # A signed opinion is not an authored edge.
        witness = json.loads(payload['body'])
        routing_memory.validate_witness(witness, expected_db_id=DB_ID)
        self.assertEqual(witness['binding'], route)
        self.assertEqual(witness['source_turn'], {'session': SESSION, 'turn': 'turn-1'})
        self.assertEqual(witness['sign'], 'weaken')
        receipt = self.state()['receipts'][-1]
        self.assertEqual(receipt['outcome'], 'verified')
        self.assertEqual(receipt['routing']['outcome'], 'included')
        self.assertEqual(receipt['proposal']['body'], witness['note'])
        self.assertNotIn('edge_fingerprint', jobs.encoded(receipt).decode())
        self.assertFalse(self.step(runtime)[0])
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(len(self.writes), 1)

    def test_delivered_route_becomes_scoped_read_hint_without_cross_topic_mutation(self):
        """Joined Python loop; authored model answers/native fixture are not a utility eval."""
        self.config.update(reader_model='gpt-6.1-sol', librarian_effort='medium')
        import hooks
        import reader_worker
        import routing_contract
        import routing_memory
        from test_hook_recall import FakeClient
        from test_routing_memory import binding, PREVIOUS, WITNESS

        store = self.root / '.mneme' / 'codex-memory.db'
        store.parent.mkdir(); store.touch()  # Never opened; FakeClient owns native I/O.
        self.service.write_text(json.dumps({'binary':str(self.root / 'not-used'),
            'project_db':str(store), 'working_directory':str(self.root),
            'state_dir':str(self.root / 'private'), 'port':18765}))
        self.begin()
        bound = binding()
        bound['db_id'] = DB_ID
        bound['route'].update(previous=PREVIOUS, target=OTHER_DB, **{'from':PREVIOUS, 'to':OTHER_DB})
        card = {'id':OTHER_DB, 'summary':'Use R6 for this mechanism.', 'status':'active',
                'source':'codex:prior-lesson', 'fingerprint':'a'*64}
        native = {'outcome':'ok', 'cards':[card], 'db_id':DB_ID,
            'observation':{'schema':1, 'learning':'disabled', 'cards':[
                {'node_id':OTHER_DB, 'graph_path':[
                    {k:bound['route'][k] for k in ('previous','target','from','to')}],
                 'routing_binding':bound}]}}

        class Selector:
            def select(self, dialogue, cards):
                return {'selected_ids':[OTHER_DB], 'reason':'selected', 'provider_attempt':True,
                        'usage':{'input_tokens':10, 'output_tokens':2}}

        with patch('hook_recall.collect_reader', return_value=native):
            selected = reader_worker._execute(self.config, PROMPT, Selector(), lambda:True, set(),
                routing=lambda:{'hints':[], 'mode':'baseline_exploration', 'stop':False})
        rendered, displayed = hooks._render_cards_with_display(selected['cards'], DB_ID)
        self.assertEqual(displayed[0]['routing_binding'], bound)
        self.assertNotIn('edge_fingerprint', rendered)
        packet = {'schema':'mneme.codex-memory-delivery.v4', 'session_id':SESSION,
            'turn_id':'turn-1', 'rendered_text':rendered,
            'rendered_sha256':hashlib.sha256(rendered.encode()).hexdigest(), 'displayed':displayed, 'concerns':[]}
        self.assertEqual(jobs.attach_delivery(self.config, packet)['outcome'], 'attached')
        self.append([source.delivered(packet, turn='turn-1')])
        self.source_complete(); jobs.close_turn(self.config, SESSION, 'turn-1')
        _, assessor = self.step(FakeRuntime(self, routing=True))
        self.assertEqual(len(assessor.calls), 1)
        self.assertEqual(len(self.writes), 1)
        payload = copy.deepcopy(self.writes[0][1]['payload'])
        history = {**payload, 'id':WITNESS, 'db_id':DB_ID, 'status':'active',
            'memory_kind':{'kind':'semantic'}, 'summary_truncated':False,
            'provenance':{'type':'external', 'source':payload['source']},
            'body_range':{'source_start':0, 'source_end':len(payload['body'].encode()),
                          'has_more':False, 'next_offset':None}}
        self.assertIsNotNone(routing_memory.decode_witness(history, expected_db_id=DB_ID))
        catalog = [{'db':'project', 'name':'project', 'state':'open',
                    'configured_path':str(store), 'db_id':DB_ID}]
        context = {'schema':'mneme.context.v6', 'core':[], 'primary':[{'id':WITNESS}],
                   'expansions':[], 'episodes':[]}
        calls, charged = [], []

        class Matcher(Selector):
            def route(self, current, witnesses, *, expected_db_id):
                calls.append(current)
                _, ctx = routing_contract.prepare(current, witnesses, expected_db_id=expected_db_id)
                result = routing_contract.validate_answer({'judgments':[{
                    'witness':'w1', 'applicability':verdict, 'current_refs':['c1'],
                    'correction_applicable':False}]}, ctx)
                return {'reason':'matched' if result.hints else 'neutral', 'routing':result,
                        'provider_attempt':True, 'usage':{'input_tokens':13, 'output_tokens':3}}

        for cue, verdict, expected in (
                ('Project X compatibility requires R5.', 'matched', ['weaken']),
                ('Unrelated project Y supports R6.', 'opposite', []),
                ('Project X rolled back; compatibility requires R5 again.', 'matched', ['weaken'])):
            client = FakeClient(catalog, context, {WITNESS:history})
            with self.subTest(cue=cue), patch('hook_recall.McpClient', return_value=client), \
                 patch('hook_recall.collect_reader', return_value=native) as final_read:
                result = reader_worker._execute(self.config, cue, Matcher(), lambda:True, set(),
                    routing=lambda:reader_worker._match_routing(self.config, cue, Matcher(),
                        lambda:True, lambda receipt:charged.append(receipt) or True))
                hints = final_read.call_args.kwargs['routing_hints']
                self.assertEqual([hint['sign'] for hint in hints], expected)
                self.assertEqual(result['cards'][0]['id'], OTHER_DB)
                for hint in hints:
                    self.assertEqual(hint, {**bound, 'sign':'weaken'})
                self.assertEqual([name for name,_ in client.calls],
                                 ['databases','recall_context','get','databases'])
                self.assertEqual(client.calls[1][1]['depth'], 0)
                self.assertEqual(client.calls[1][1]['tags'], [routing_memory.TAG])
        self.assertEqual(len(charged), 3)
        self.assertEqual(self.writes[0][1]['payload'], payload)
        self.assertEqual(len(self.writes), 1)  # Reads never update a base weight or note.

    def test_sensitive_optional_rationale_omits_only_routing_not_safe_note(self):
        from dataclasses import replace
        self.routed_closed()

        class OptionalSensitive(FakeRuntime):
            def assess(self, *args, **kwargs):
                result = super().assess(*args, **kwargs)
                proposal = result['proposal']
                result['proposal'] = replace(proposal, routing_judgment=replace(
                    proposal.routing_judgment, rationale='sk-' + 'syntheticNotASecret'*2))
                return result

        _, runtime = self.step(OptionalSensitive(self, routing=True))
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(len(self.writes), 1)
        payload = self.writes[0][1]['payload']
        self.assertEqual(payload['source']['namespace'], 'codex-acquisition.v1')
        self.assertNotIn('tags', payload)
        self.assertNotIn('syntheticNotASecret', jobs.encoded(payload).decode())
        receipt = self.state()['receipts'][-1]
        self.assertEqual(receipt['outcome'], 'verified')
        self.assertEqual(receipt['routing'], {'outcome':'omitted', 'reason':'routing_sensitive'})

    def test_returned_but_unadmitted_packet_keeps_ordinary_recording(self):
        self.begin()
        packet = self.delivery_packet()
        self.assertEqual(jobs.attach_delivery(self.config, packet)["outcome"], "attached")
        self.source_complete()
        jobs.close_turn(self.config, SESSION, "turn-1")
        _, runtime = self.step(FakeRuntime(self))
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(len(self.writes), 1)
        self.assertEqual(runtime.calls[0][0]["coverage"]["memory_delivery"], "not_admitted")
        self.assertNotIn("links", self.writes[0][1]["payload"])
        self.assertNotIn(packet["rendered_text"], jobs.encoded(self.state()).decode())

    def test_stale_delivered_target_saves_historical_note_without_link(self):
        self.begin()
        packet = self.delivery_packet()
        self.assertEqual(jobs.attach_delivery(self.config, packet)["outcome"], "attached")
        self.append([source.row("response_item", {
            "type": "message", "id": "hook-packet", "role": "developer",
            "content": [{"type": "input_text", "text": packet["rendered_text"]}],
            source.META: {"turn_id": "turn-1", "content_item_kinds": ["hooks.additional_context"]}})])
        self.source_complete()
        jobs.close_turn(self.config, SESSION, "turn-1")
        with patch("hook_recall.check_association_target", return_value="stale") as check:
            self.step(FakeRuntime(self, associate=True))
        check.assert_called_once()
        self.assertEqual(len(self.writes), 1)
        self.assertNotIn("links", self.writes[0][1]["payload"])
        self.assertEqual(self.state()["receipts"][-1]["association"],
                         {"target": OTHER_DB, "outcome": "omitted", "reason": "stale"})

    def test_completed_source_is_not_new_even_after_startup(self):
        self.source_start()
        self.source_complete()
        self.startup()
        self.assertEqual(jobs.notice(self.config, self.event())["outcome"], "deferred")
        self.assertEqual(self.state()["jobs"], [])

    def test_without_lifecycle_or_retired_resume_first_prompt_only_baselines(self):
        self.source_start()
        self.assertEqual(jobs.notice(self.config, self.event())["outcome"], "baseline_only")
        self.assertEqual(self.state()["jobs"], [])
        self.source_complete()
        self.source_start("turn-2")
        self.assertEqual(jobs.notice(self.config, self.event("turn-2"))["outcome"], "admitted")

    def test_terminal_reclamation_is_not_a_lifetime_64_turn_quota(self):
        self.startup()
        for index in range(jobs.MAX_JOBS + 5):
            turn = f"turn-{index}"
            self.source_start(turn)
            self.assertEqual(jobs.notice(self.config, self.event(turn))["outcome"], "admitted")
            self.mutate(lambda data: jobs._finish(data, data["jobs"][0], "abstained", "synthetic_terminal"))
            self.source_complete(turn)
        state = self.state()
        self.assertEqual(state["jobs"], [])
        self.assertEqual(state["counts"]["admitted"], jobs.MAX_JOBS + 5)
        self.assertEqual(state["counts"]["abstained"], jobs.MAX_JOBS + 5)
        self.assertLessEqual(len(state["receipts"]), jobs.MAX_RECEIPTS)
        self.assertGreater(state["watermark"], 0)

    def test_outstanding_capacity_never_evicts_unresolved_jobs(self):
        self.startup()
        for index in range(jobs.MAX_JOBS):
            turn = f"turn-{index}"
            self.source_start(turn)
            self.assertEqual(jobs.notice(self.config, self.event(turn))["outcome"], "admitted")
            self.source_complete(turn)
        self.mutate(lambda data: data["jobs"][0].update(phase="unresolved", reason="ambiguous"))
        retained = self.state()["jobs"]
        self.source_start("over-cap")
        self.assertEqual(jobs.notice(self.config, self.event("over-cap"))["outcome"], "capacity")
        state = self.state()
        self.assertEqual(state["jobs"], retained)
        self.assertEqual(state["counts"]["capacity"], 1)

    def test_more_than_16_ended_sessions_reclaim_without_resetting_reader_state(self):
        self.config["state_dir"].mkdir(parents=True)
        reader = self.config["state_dir"] / "reader-budget-sentinel.json"
        reader.write_bytes(b'{"attempts":48,"input_tokens":250000}')
        for index in range(jobs.MAX_LEDGERS + 5):
            session = f"ended-{index}"
            self.begin(session=session)
            jobs.close_turn(self.config, session, cancel=True, ended=True)
        self.assertLessEqual(len(list(jobs._directory(self.config).glob("*.json"))), jobs.MAX_LEDGERS)
        self.assertEqual(reader.read_bytes(), b'{"attempts":48,"input_tokens":250000}')

    def test_retired_resume_bootstraps_existing_bytes_then_allows_later_start(self):
        self.begin()
        self.source_complete()
        jobs.close_turn(self.config, SESSION, cancel=True, ended=True)
        for index in range(jobs.MAX_LEDGERS):
            self.startup(f"live-{index}")
        self.assertFalse(jobs._path(self.config, SESSION).exists())
        jobs.close_turn(self.config, "live-0", ended=True)
        self.source_start("resume-baseline")
        self.startup(source_name="resume")
        self.assertEqual(jobs.notice(self.config, self.event("resume-baseline"))["outcome"], "baseline_only")
        self.assertFalse(self.state()["fresh"])
        self.assertEqual(self.state()["jobs"], [])
        self.source_complete("resume-baseline")
        self.source_start("after-resume")
        self.assertEqual(jobs.notice(self.config, self.event("after-resume"))["outcome"], "admitted")

    def test_rotation_refuses_new_jobs_without_resetting_existing_pin(self):
        self.begin()
        before = self.state()
        replacement = self.sessions / "replacement.jsonl"
        replacement.write_bytes(source.encoded([source.header(SESSION), source.start("rotated"),
                                                source.user(PROMPT, turn="rotated")]))
        replacement.replace(self.path())
        self.assertEqual(jobs.notice(self.config, self.event("rotated"))["outcome"], "source_changed")
        self.assertEqual(self.state(), before)

    def test_stop_and_session_end_preserve_pending_writer_and_original_deadline(self):
        self.begin()
        jobs.close_turn(self.config, SESSION, "turn-1")
        first = self.state()["jobs"][0]
        self.now += 3
        jobs.close_turn(self.config, SESSION, ended=True)
        state = self.state()
        self.assertTrue(state["ended"])
        self.assertEqual(state["jobs"][0]["deadline"], first["deadline"])
        self.assertTrue(jobs.has_work(self.config, SESSION))
        self.source_complete()
        self.step()
        self.assertEqual(self.state()["counts"]["verified"], 1)
        self.assertEqual(len(self.writes), 1)

    def test_durable_reservation_and_write_intent_precede_external_calls(self):
        self.closed()
        _, runtime = self.step()
        state = self.state()
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(len(self.reservations), 1)
        self.assertEqual(len(self.accounts), 1)
        self.assertEqual(len(self.writes), 1)
        self.assertEqual(state["counts"]["assessed"], 1)
        self.assertEqual(state["counts"]["verified"], 1)
        self.assertEqual(state["jobs"], [])
        written = self.writes[0][1]
        self.assertEqual(written["db_id"], DB_ID)
        self.assertEqual(written["payload"]["source"]["namespace"], "codex-acquisition.v1")
        self.assertNotIn("evidence", written)
        self.assertNotIn("observation", written)
        self.assertNotIn("action", written["payload"])

    def test_episode_proposal_freezes_append_not_revision_or_links(self):
        self.closed()
        self.step(FakeRuntime(self, kind="episode"))
        payload = self.writes[0][1]["payload"]
        self.assertEqual(payload["action"], "append")
        self.assertFalse({"links", "revises", "supersedes", "core"} & payload.keys())

    def test_null_proposal_is_terminal_without_native_write(self):
        self.closed()
        _, runtime = self.step(FakeRuntime(self, reason="abstained"))
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(self.state()["counts"]["abstained"], 1)
        self.assertEqual(self.state()["jobs"], [])
        self.assertEqual(self.writes, [])
        self.assertFalse(self.step(runtime)[0])

    def test_shared_budget_refusal_happens_before_model_or_write(self):
        self.closed()
        _, runtime = self.step(reserve=lambda key: False)
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.accounts, [])
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["counts"]["deferred"], 1)

    def test_account_failure_preserves_ambiguity_and_prohibits_native_write(self):
        self.closed()
        _, runtime = self.step(account=lambda key, result: False)
        self.assertEqual(len(runtime.calls), 1)
        state = self.state()
        self.assertEqual(state["jobs"][0]["phase"], "unresolved")
        self.assertTrue(state["usage_unknown"])
        self.assertEqual(self.writes, [])
        self.assertFalse(self.step(runtime)[0])

    def test_crashed_assessment_intent_never_buys_another_provider_call(self):
        self.closed()
        self.mutate(lambda data: data["jobs"][0].update(phase="assessing", db_id=DB_ID))
        _, runtime = self.step()
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.reservations, [])
        self.assertEqual(self.writes, [])
        self.assertTrue(self.state()["usage_unknown"])
        self.assertEqual(self.state()["jobs"][0]["phase"], "unresolved")
        self.assertEqual(self.accounts[0][1], {"provider_attempt": True, "usage": None})
        self.assertFalse(self.step(runtime)[0])

    def test_failed_assessment_intent_persistence_prevents_reservation_or_model(self):
        self.closed()
        original_write = jobs._write
        def write(path, data):
            if any(job["phase"] == "assessing" for job in data["jobs"]):
                raise OSError("synthetic fsync failure")
            return original_write(path, data)
        with patch("recording_jobs._write", side_effect=write):
            _, runtime = self.step()
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.reservations, [])
        self.assertEqual(self.accounts, [])
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["jobs"][0]["phase"], "observing")

    def test_provider_exception_leaves_unknown_usage_without_retry(self):
        self.closed()
        runtime = FakeRuntime(self)
        runtime.assess = lambda *args, **kwargs: (_ for _ in ()).throw(RuntimeError("synthetic crash"))
        self.step(runtime)
        self.assertTrue(self.state()["usage_unknown"])
        self.assertEqual(self.state()["jobs"][0]["phase"], "unresolved")
        self.assertEqual(self.writes, [])
        self.assertFalse(self.step(runtime)[0])

    def test_interrupt_does_not_erase_external_assessment_intent(self):
        self.closed()
        self.mutate(lambda data: data["jobs"][0].update(phase="assessing", db_id=DB_ID))
        jobs.close_turn(self.config, SESSION, cancel=True, ended=True)
        state = self.state()
        self.assertEqual(len(state["jobs"]), 1)
        self.assertIn(state["jobs"][0]["phase"], ("assessing", "unresolved"))
        self.step()
        self.assertEqual(self.state()["jobs"][0]["phase"], "unresolved")

    def test_cancellation_during_overlap_prevents_reservation_and_assessment(self):
        self.closed()
        def cancel(*args, **kwargs):
            jobs.close_turn(self.config, SESSION, cancel=True)
            return {"outcome": "empty", "cards": [], "db_id": DB_ID}
        self.overlap.side_effect = cancel
        _, runtime = self.step()
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.reservations, [])
        self.assertEqual(self.accounts, [])
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["counts"]["cancelled"], 1)

    def test_held_lock_interrupt_during_overlap_still_prevents_assessment(self):
        self.closed()
        interrupts = []
        def overlap(*args, **kwargs):
            interrupts.append(self.interrupt_with_queue_lock())
            return {"outcome": "empty", "cards": [], "db_id": DB_ID}
        self.overlap.side_effect = overlap
        _, runtime = self.step()
        self.assertEqual(len(interrupts), 1)
        self.assertLess(interrupts[0]["elapsed"], 0.5)
        self.assertRegex(interrupts[0]["marker"]["token"], r"^[0-9a-f]{64}$")
        self.assertEqual(interrupts[0]["marker"]["turn_id"], "turn-1")
        self.assertEqual(interrupts[0]["state"]["jobs"][0]["phase"], "observing")
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.reservations, [])
        self.assertEqual(self.accounts, [])
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["jobs"], [])
        self.assertEqual(self.state()["counts"]["cancelled"], 1)

    def test_held_lock_interrupt_compacts_admitted_work_on_next_wake_without_stop(self):
        self.begin()
        interrupted = self.interrupt_with_queue_lock()
        self.assertEqual(interrupted["state"]["jobs"][0]["phase"], "admitted")
        worked, runtime = self.step()
        self.assertTrue(worked)
        self.assertEqual(runtime.calls, [])
        self.identity.assert_not_called()
        self.overlap.assert_not_called()
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["jobs"], [])
        self.assertEqual(self.state()["counts"]["cancelled"], 1)

    def test_interrupt_after_reservation_settles_unsent_attempt_without_assessment(self):
        self.closed()
        interrupts = []
        def reserve(key):
            granted = self.reserve(key)
            interrupts.append(self.interrupt_with_queue_lock())
            return granted
        _, runtime = self.step(reserve=reserve)
        self.assertEqual(len(interrupts), 1)
        self.assertEqual(len(self.reservations), 1)
        self.assertEqual(runtime.calls, [])
        self.assertEqual(len(self.accounts), 1)
        self.assertEqual(self.accounts[0][1], {"provider_attempt": False})
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["jobs"], [])
        self.assertEqual(self.state()["counts"]["cancelled"], 1)

    def test_interrupt_after_reservation_with_failed_release_stays_unresolved(self):
        self.closed()
        interrupts = []
        def reserve(key):
            granted = self.reserve(key)
            interrupts.append(self.interrupt_with_queue_lock())
            return granted
        def account(key, result):
            self.accounts.append((key, result))
            return False
        _, runtime = self.step(reserve=reserve, account=account)
        self.assertEqual(len(interrupts), 1)
        self.assertEqual(len(self.reservations), 1)
        self.assertEqual(runtime.calls, [])
        self.assertEqual(len(self.accounts), 1)
        self.assertEqual(self.accounts[0][1], {"provider_attempt": False})
        self.assertEqual(self.writes, [])
        state = self.state()
        self.assertTrue(state["usage_unknown"])
        self.assertEqual(state["jobs"][0]["phase"], "unresolved")
        self.assertEqual(state["jobs"][0]["reason"], "accounting_unavailable")
        self.assertEqual(state["counts"]["cancelled"], 0)
        self.assertFalse(self.step(runtime)[0])
        self.assertEqual(len(self.accounts), 1)

    def test_held_lock_interrupt_during_assessment_settles_usage_and_discards_proposal(self):
        self.closed()
        runtime = FakeRuntime(self)
        assess = runtime.assess
        interrupts = []
        def interrupted(*args, **kwargs):
            result = assess(*args, **kwargs)
            interrupts.append(self.interrupt_with_queue_lock())
            return result
        runtime.assess = interrupted
        self.step(runtime)
        self.assertEqual(len(interrupts), 1)
        self.assertEqual(interrupts[0]["state"]["jobs"][0]["phase"], "assessing")
        self.assertNotIn("cancel_requested", interrupts[0]["state"]["jobs"][0])
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(len(self.accounts), 1)
        self.assertEqual(self.accounts[0][1]["usage"], {"input_tokens": 20, "output_tokens": 5})
        self.assertEqual(self.writes, [])
        state = self.state()
        self.assertFalse(state["usage_unknown"])
        self.assertEqual(state["jobs"], [])
        self.assertEqual(state["counts"]["cancelled"], 1)
        self.assertNotIn("proposal", state["receipts"][-1])
        self.assertFalse(self.step(runtime)[0])
        self.assertEqual(len(self.accounts), 1)

    def test_held_lock_interrupt_with_failed_assessment_accounting_stays_unresolved(self):
        self.closed()
        runtime = FakeRuntime(self)
        assess = runtime.assess
        interrupts = []
        def interrupted(*args, **kwargs):
            result = assess(*args, **kwargs)
            interrupts.append(self.interrupt_with_queue_lock())
            return result
        runtime.assess = interrupted
        self.step(runtime, account=lambda key, result: False)
        self.assertEqual(len(interrupts), 1)
        self.assertEqual(len(runtime.calls), 1)
        self.assertTrue(self.state()["usage_unknown"])
        self.assertEqual(self.state()["jobs"][0]["phase"], "unresolved")
        self.assertEqual(self.state()["counts"]["cancelled"], 0)
        self.assertEqual(self.writes, [])
        self.assertFalse(self.step(runtime)[0])

    def test_interrupt_at_frozen_persistence_boundary_prevents_native_send(self):
        self.closed()
        original_write = jobs._write
        interrupts = []
        def write(path, data):
            original_write(path, data)
            if not interrupts and any(job["phase"] == "frozen" for job in data["jobs"]):
                # The real _transaction already holds the queue lock here.
                jobs.close_turn(self.config, SESSION, "turn-1", cancel=True)
                interrupts.append({"marker": jobs._cancel_marker(self.config, SESSION),
                                   "state": self.state()})
        with patch("recording_jobs._write", side_effect=write):
            _, runtime = self.step()
        self.assertEqual(len(interrupts), 1)
        self.assertEqual(interrupts[0]["state"]["jobs"][0]["phase"], "frozen")
        self.assertRegex(interrupts[0]["marker"]["token"], r"^[0-9a-f]{64}$")
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(len(self.accounts), 1)
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["jobs"], [])
        self.assertEqual(self.state()["counts"]["cancelled"], 1)

    def test_interrupt_after_write_intent_persists_but_before_send_cancels_unsent_write(self):
        self.closed()
        original_write = jobs._write
        interrupts = []
        def write(path, data):
            original_write(path, data)
            if not interrupts and any(job["phase"] == "write_intent" for job in data["jobs"]):
                jobs.close_turn(self.config, SESSION, "turn-1", cancel=True)
                interrupts.append({"marker": jobs._cancel_marker(self.config, SESSION),
                                   "state": self.state()})
        with patch("recording_jobs._write", side_effect=write):
            _, runtime = self.step()
        self.assertEqual(len(interrupts), 1)
        self.assertEqual(interrupts[0]["state"]["jobs"][0]["phase"], "write_intent")
        self.assertRegex(interrupts[0]["marker"]["token"], r"^[0-9a-f]{64}$")
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["jobs"], [])
        self.assertEqual(self.state()["counts"]["cancelled"], 1)
        self.assertFalse(self.step(runtime)[0])

    def test_delayed_notice_for_cancelled_turn_cannot_admit(self):
        self.startup()
        self.source_start()
        interrupted = self.interrupt_with_queue_lock()
        self.assertEqual(interrupted["marker"]["turn_id"], "turn-1")
        result = jobs.notice(self.config, self.event())
        self.assertNotEqual(result["outcome"], "admitted")
        self.assertEqual(self.state()["jobs"], [])
        self.assertEqual(self.state()["counts"]["admitted"], 0)
        self.identity.assert_not_called()
        self.overlap.assert_not_called()

    def test_later_genuine_turn_pins_current_cancel_token_and_can_complete(self):
        self.begin()
        jobs.close_turn(self.config, SESSION, "turn-1", cancel=True)
        marker = jobs._cancel_marker(self.config, SESSION)
        self.source_complete()
        self.source_start("turn-2")
        self.assertEqual(jobs.notice(self.config, self.event("turn-2"))["outcome"], "admitted")
        self.assertEqual(self.state()["jobs"][0]["cancel_token"], marker["token"])
        self.source_complete("turn-2")
        jobs.close_turn(self.config, SESSION, "turn-2")
        _, runtime = self.step()
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(len(self.writes), 1)
        self.assertEqual(self.state()["counts"]["verified"], 1)

    def test_stale_interrupt_conservatively_cancels_newer_admitted_work(self):
        self.begin()
        jobs.close_turn(self.config, SESSION, "turn-1", cancel=True)
        self.source_complete()
        self.source_start("turn-2")
        self.assertEqual(jobs.notice(self.config, self.event("turn-2"))["outcome"], "admitted")
        self.source_complete("turn-2")
        jobs.close_turn(self.config, SESSION, "turn-2")
        self.interrupt_with_queue_lock("turn-1")
        _, runtime = self.step()
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["jobs"], [])
        self.assertEqual(self.state()["counts"]["cancelled"], 2)

    def test_missing_or_invalid_job_cancel_pin_fails_closed(self):
        self.closed()
        original = self.state()["jobs"][0]
        for pin in (None, 42, "invalid", "a" * 64):
            with self.subTest(pin=pin):
                replacement = dict(original)
                if pin is None:
                    replacement.pop("cancel_token")
                else:
                    replacement["cancel_token"] = pin
                self.mutate(lambda data: data.update(jobs=[replacement]))
                _, runtime = self.step()
                self.assertEqual(runtime.calls, [])
                self.assertEqual(self.state()["jobs"], [])
        self.assertEqual(self.reservations, [])
        self.assertEqual(self.accounts, [])
        self.assertEqual(self.writes, [])

    def test_removing_nonempty_marker_cancels_pinned_work(self):
        self.startup()
        jobs.close_turn(self.config, SESSION, "prior-turn", cancel=True)
        self.closed()
        self.assertRegex(self.state()["jobs"][0]["cancel_token"], r"^[0-9a-f]{64}$")
        jobs._cancel_path(self.config, SESSION).unlink()
        _, runtime = self.step()
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["counts"]["cancelled"], 1)

    def test_cancel_marker_is_strict_and_invalid_marker_refuses_admission(self):
        self.startup()
        self.source_start()
        path = jobs._cancel_path(self.config, SESSION)
        self.assertEqual(jobs._cancel_marker(self.config, SESSION), {"token": "", "turn_id": None})
        valid = {"token": "a" * 64, "turn_id": "prior-turn"}
        bad_values = [b'{', b'null', jobs.encoded({}), jobs.encoded({**valid, "token": ""}),
                      jobs.encoded({**valid, "token": "A" * 64}),
                      jobs.encoded({**valid, "turn_id": 2}),
                      jobs.encoded({**valid, "extra": True}), b' ' * 257]
        for raw in bad_values:
            with self.subTest(raw=raw):
                path.write_bytes(raw)
                self.assertIsNone(jobs._cancel_marker(self.config, SESSION))
                self.assertNotEqual(jobs.notice(self.config, self.event())["outcome"], "admitted")
                self.assertEqual(self.state()["jobs"], [])
        path.write_bytes(jobs.encoded(valid))
        self.assertEqual(jobs._cancel_marker(self.config, SESSION), valid)

    def test_invalid_marker_after_admission_cancels_before_assessment(self):
        self.closed()
        jobs._cancel_path(self.config, SESSION).write_bytes(b'{"token":null}')
        _, runtime = self.step()
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.reservations, [])
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["jobs"], [])
        self.assertEqual(self.state()["counts"]["cancelled"], 1)

    def test_clean_ledger_retirement_removes_its_cancel_marker(self):
        self.begin()
        jobs.close_turn(self.config, SESSION, "turn-1", cancel=True, ended=True)
        marker_path = jobs._cancel_path(self.config, SESSION)
        self.assertTrue(marker_path.exists())
        for index in range(jobs.MAX_LEDGERS):
            self.startup(f"retire-{index}")
        self.assertFalse(jobs._path(self.config, SESSION).exists())
        self.assertFalse(marker_path.exists())

    def test_orphan_marker_cleanup_never_removes_active_ledger_marker(self):
        self.startup()
        jobs.close_turn(self.config, SESSION, "prior-turn", cancel=True)
        marker_path = jobs._cancel_path(self.config, SESSION)
        marker_bytes = marker_path.read_bytes()
        orphan = jobs._cancel_path(self.config, "orphan")
        orphan.write_bytes(jobs.encoded({"token": "b" * 64, "turn_id": None}))
        self.assertFalse(jobs._path(self.config, "orphan").exists())
        self.startup("new-ledger")
        self.assertFalse(orphan.exists())
        self.assertEqual(marker_path.read_bytes(), marker_bytes)

    def test_orphan_cleanup_overflow_cannot_infer_unseen_ledger_capacity(self):
        self.startup()
        jobs.close_turn(self.config, SESSION, "prior-turn", cancel=True)
        marker_path = jobs._cancel_path(self.config, SESSION)
        marker_bytes = marker_path.read_bytes()
        orphans = [jobs._cancel_path(self.config, f"orphan-{i}") for i in range(132)]
        for path in orphans:
            path.write_bytes(jobs.encoded({"token": "b" * 64, "turn_id": None}))
        self.startup("after-overflow")
        removed = sum(not path.exists() for path in orphans)
        self.assertGreater(removed, 0)
        self.assertLessEqual(removed, 130)
        self.assertFalse(jobs._path(self.config, "after-overflow").exists())
        self.assertEqual(marker_path.read_bytes(), marker_bytes)
        # A later genuine event may finish cleanup; no background polling required.
        self.startup("after-overflow")
        self.assertTrue(jobs._path(self.config, "after-overflow").exists())
        self.assertTrue(all(not path.exists() for path in orphans))
        self.assertEqual(marker_path.read_bytes(), marker_bytes)

    def test_interrupt_inside_native_call_keeps_verified_receipt_without_retry(self):
        self.closed()
        interrupts = []
        def native(config, job, timeout):
            interrupts.append(self.interrupt_with_queue_lock())
            return self.native_write(config, job, timeout)
        _, runtime = self.step(native_write=native)
        self.assertEqual(len(interrupts), 1)
        self.assertEqual(interrupts[0]["state"]["jobs"][0]["phase"], "write_intent")
        self.assertEqual(len(self.writes), 1)
        state = self.state()
        self.assertEqual(state["counts"]["verified"], 1)
        self.assertEqual(state["counts"]["cancelled"], 0)
        self.assertEqual(state["jobs"], [])
        self.assertEqual(state["receipts"][-1]["native"]["readback_status"], "verified")
        self.assertFalse(self.step(runtime, native_write=native)[0])
        self.assertEqual(len(self.writes), 1)

    def test_cancellation_cannot_turn_unknown_native_intent_into_cancelled(self):
        self.closed()
        self.mutate(lambda data: data["jobs"][0].update(phase="write_intent", db_id=DB_ID))
        self.interrupt_with_queue_lock()
        _, runtime = self.step()
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["jobs"][0]["phase"], "unresolved")
        self.assertEqual(self.state()["counts"]["cancelled"], 0)
        self.assertFalse(self.step(runtime)[0])

    def test_frozen_unattempted_write_recovers_without_reassessment(self):
        self.closed()
        payload = {"source": {"namespace": "codex-acquisition.v1", "key": "frozen-source"},
                   "summary": "Already frozen.", "body": "A bounded frozen note."}
        self.mutate(lambda data: data["jobs"][0].update(
            phase="frozen", db_id=DB_ID, proposal_kind="lesson", payload=payload, citations=[]))
        _, runtime = self.step()
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.reservations, [])
        self.assertEqual(self.accounts, [])
        self.assertEqual(self.writes[0][1]["payload"], payload)
        self.assertEqual(self.state()["counts"]["verified"], 1)
        self.identity.assert_not_called()
        self.overlap.assert_not_called()

    def test_crashed_write_intent_is_unresolved_not_replayed(self):
        self.closed()
        self.mutate(lambda data: data["jobs"][0].update(phase="write_intent", db_id=DB_ID))
        self.step()
        self.assertEqual(self.state()["jobs"][0]["phase"], "unresolved")
        self.assertEqual(self.writes, [])
        self.assertFalse(self.step()[0])

    def test_failed_write_intent_persistence_preserves_frozen_recovery(self):
        self.closed()
        original_write = jobs._write
        def write(path, data):
            if any(job["phase"] == "write_intent" for job in data["jobs"]):
                raise OSError("synthetic fsync failure")
            return original_write(path, data)
        with patch("recording_jobs._write", side_effect=write):
            _, runtime = self.step()
        self.assertEqual(len(runtime.calls), 1)
        frozen = self.state()["jobs"][0]
        self.assertEqual(frozen["phase"], "frozen")
        self.assertEqual(self.writes, [])
        self.step(runtime)
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(len(self.reservations), 1)
        self.assertEqual(len(self.accounts), 1)
        self.assertEqual(self.writes[0][1]["payload"], frozen["payload"])
        self.assertEqual(self.state()["counts"]["verified"], 1)

    def test_native_ambiguous_or_wrong_identity_receipt_never_reports_saved(self):
        for receipt in (None, {"db": "project", "db_id": DB_ID, "accepted": True},
                        {"db": "user", "db_id": DB_ID, "readback_status": "verified"},
                        {"db": "project", "db_id": OTHER_DB, "readback_status": "verified"}):
            with self.subTest(receipt=receipt):
                # Reuse one source with a synthetic pre-write crash fixture.
                if not jobs._path(self.config, SESSION).exists():
                    self.closed()
                self.mutate(lambda data: data["jobs"][0].update(
                    phase="frozen", db_id=DB_ID, proposal_kind="lesson", payload={}, citations=[]))
                calls = []
                self.step(native_write=lambda *args: calls.append(args) or receipt)
                self.assertEqual(len(calls), 1)
                self.assertEqual(self.state()["jobs"][0]["phase"], "unresolved")
                self.assertEqual(self.state()["counts"]["verified"], 0)
                self.assertFalse(self.step()[0])

    def test_native_acceptance_then_exception_is_not_retried(self):
        self.closed()
        calls = []
        def ambiguous(*args):
            calls.append(args)
            raise RuntimeError("accepted remotely, reply lost")
        self.step(native_write=ambiguous)
        self.assertEqual(len(calls), 1)
        self.assertEqual(self.state()["jobs"][0]["phase"], "unresolved")
        self.assertFalse(self.step(native_write=ambiguous)[0])
        self.assertEqual(len(calls), 1)

    def semantic_overlap(self):
        self.overlap.return_value = {"outcome": "ok", "db_id": DB_ID,
                                     "cards": [{"id": OTHER_DB, "summary": "Earlier mechanism.",
                                                "kind": "semantic"}]}

    def test_one_selected_association_is_requested_in_atomic_capture_not_claimed_linked(self):
        self.closed()
        self.semantic_overlap()
        with patch("hook_recall.check_association_target", return_value="kept") as check:
            _, runtime = self.step(FakeRuntime(self, associate=True))
        check.assert_called_once()
        self.assertEqual(check.call_args.kwargs["expected_db_id"], DB_ID)
        self.assertLessEqual(check.call_args.kwargs["timeout"], 2)
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(len(self.accounts), 1)
        self.assertEqual(len(self.writes), 1)
        self.assertEqual(self.writes[0][1]["payload"]["links"], [
            {"to": OTHER_DB, "kind": "associative", "weight": 0.5}])
        receipt = self.state()["receipts"][-1]
        self.assertEqual(receipt["association"], {"target": OTHER_DB,
                                                "outcome": "requested", "reason": "kept"})
        self.assertNotIn("linked", jobs.encoded(receipt).decode())

    def test_stale_or_unavailable_association_drops_only_optional_link(self):
        for disposition in ("stale", "unavailable"):
            with self.subTest(disposition=disposition):
                self.closed()
                self.semantic_overlap()
                with patch("hook_recall.check_association_target", return_value=disposition) as check:
                    self.step(FakeRuntime(self, associate=True))
                check.assert_called_once()
                self.assertEqual(len(self.writes), 1)
                self.assertNotIn("links", self.writes[0][1]["payload"])
                self.assertEqual(self.state()["receipts"][-1]["association"],
                                 {"target": OTHER_DB, "outcome": "omitted", "reason": disposition})
                jobs._path(self.config, SESSION).unlink()
                self.path().unlink()
                self.writes.clear(); self.accounts.clear(); self.reservations.clear()
                self.overlap.reset_mock(return_value=True)

    def test_association_check_interrupt_cancels_before_freeze_or_native_write(self):
        self.closed()
        self.semantic_overlap()
        def interrupt(*args, **kwargs):
            jobs.close_turn(self.config, SESSION, "turn-1", cancel=True)
            return "kept"
        with patch("hook_recall.check_association_target", side_effect=interrupt) as check:
            self.step(FakeRuntime(self, associate=True))
        check.assert_called_once()
        self.assertEqual(self.state()["receipts"][-1]["outcome"], "cancelled")
        self.assertNotIn("association", self.state()["receipts"][-1])
        self.assertEqual(self.writes, [])
        self.assertEqual(len(self.accounts), 1)

    def test_requested_link_on_native_refusal_is_not_success_or_changed_retry(self):
        self.closed()
        self.semantic_overlap()
        sends = []
        def refuse(_config, frozen, _timeout):
            sends.append(json.loads(json.dumps(frozen["payload"])))
            raise RuntimeError("native incident capacity or deletion refusal")
        with patch("hook_recall.check_association_target", return_value="kept") as check:
            self.step(FakeRuntime(self, associate=True), native_write=refuse)
            self.assertEqual(len(sends), 1)
            self.assertEqual(self.state()["jobs"][0]["phase"], "unresolved")
            self.assertEqual(self.state()["jobs"][0]["association"]["outcome"], "requested")
            self.assertEqual(self.state()["jobs"][0]["payload"], sends[0])
            self.assertFalse(self.step(native_write=refuse)[0])
            check.assert_called_once()
        self.assertEqual(len(sends), 1)
        self.assertEqual(self.state()["counts"]["verified"], 0)

    def test_native_accepted_error_receipt_stays_bounded_private_and_unresolved(self):
        self.closed()
        failure = RuntimeError("native accepted but readback failed")
        failure.details = {"accepted": {"db": "project", "db_id": DB_ID, "id": OTHER_DB,
                           "readback_status": "failed", "retryable": False,
                           "body": "UNBOUNDED_PRIVATE_NATIVE_BODY"}}
        def ambiguous(*args):
            raise failure
        self.step(native_write=ambiguous)
        job = self.state()["jobs"][0]
        self.assertEqual(job["phase"], "unresolved")
        self.assertEqual(job["receipt"]["id"], OTHER_DB)
        self.assertFalse(job["receipt"]["retryable"])
        self.assertNotIn("body", job["receipt"])
        self.assertEqual(self.state()["counts"]["verified"], 0)
        self.assertFalse(self.step(native_write=ambiguous)[0])
        self.assertTrue(jobs.export_receipts(self.config, [SESSION])["unknown"])

    def test_overlap_failure_or_identity_drift_defers_before_inference(self):
        for result in ({"outcome": "unavailable", "cards": []},
                       {"outcome": "timeout", "cards": []},
                       {"outcome": "empty", "cards": [], "db_id": OTHER_DB}):
            with self.subTest(result=result):
                self.closed()
                self.overlap.return_value = result
                _, runtime = self.step()
                self.assertEqual(runtime.calls, [])
                self.assertEqual(self.reservations, [])
                self.assertEqual(self.writes, [])
                self.assertEqual(self.state()["jobs"], [])
                # New isolated logical ledger/source for this table row.
                jobs._path(self.config, SESSION).unlink()
                self.path().unlink()

    def test_config_drift_defers_before_native_identity_or_provider_work(self):
        self.closed()
        self.service.write_text('{"changed":true}')
        _, runtime = self.step()
        self.assertEqual(runtime.calls, [])
        self.identity.assert_not_called()
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["counts"]["deferred"], 1)

    def test_overlap_finishing_after_job_deadline_cannot_start_assessment(self):
        self.closed()
        def late_overlap(*args, **kwargs):
            self.now += jobs.CLOSE_SECONDS + 1
            return {"outcome": "empty", "cards": [], "db_id": DB_ID}
        self.overlap.side_effect = late_overlap
        _, runtime = self.step()
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.reservations, [])
        self.assertEqual(self.accounts, [])
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["jobs"], [])

    def test_job_deadline_or_backward_clock_is_not_renewed(self):
        for now in (1000 + jobs.CLOSE_SECONDS + 1, 999):
            with self.subTest(now=now):
                self.now = 1000
                self.closed()
                deadline = self.state()["jobs"][0]["deadline"]
                self.now = now
                jobs.close_turn(self.config, SESSION, ended=True)
                self.assertEqual(self.state()["jobs"][0]["deadline"], deadline)
                _, runtime = self.step()
                self.assertEqual(runtime.calls, [])
                self.assertEqual(self.state()["jobs"], [])
                jobs._path(self.config, SESSION).unlink()
                self.path().unlink()

    def test_observation_caps_defer_without_native_or_provider_calls(self):
        self.closed()
        result = {"status": "deferred", "reason": "evidence_bytes_exceeded",
                  "work": {"bytes_read": 1000, "records_parsed": 10}}
        with patch("recording_jobs.observe_source_turn", return_value=result):
            _, runtime = self.step()
        self.assertEqual(runtime.calls, [])
        self.identity.assert_not_called()
        self.assertEqual(self.state()["counts"]["deferred"], 1)

    def test_flush_retry_count_is_bounded_and_persisted(self):
        self.begin()
        jobs.close_turn(self.config, SESSION, "turn-1")
        charge = jobs.observation_byte_ceiling(jobs._admission(self.state()["jobs"][0]["admission"]))
        self.assertLessEqual(jobs.MAX_POLLS * charge, jobs.MAX_SCAN_WORK)
        for index in range(jobs.MAX_POLLS):
            self.step()
            if self.state()["jobs"]:
                current = self.state()["jobs"][0]
                self.assertEqual(current["polls"], index + 1)
                self.assertEqual(current["reserved_bytes"], (index + 1) * charge)
                self.assertLessEqual(current["bytes_read"], current["reserved_bytes"])
            self.now += 0.9
        self.assertEqual(self.state()["jobs"], [])
        self.assertEqual(self.state()["counts"]["deferred"], 1)
        self.identity.assert_not_called()

    def test_scan_charge_precedes_read_and_failed_progress_cannot_buy_free_scans(self):
        self.closed()
        charge = jobs.observation_byte_ceiling(jobs._admission(self.state()["jobs"][0]["admission"]))
        original_update = jobs._update
        reads = []
        after_read = False
        def observe(*args, **kwargs):
            nonlocal after_read
            current = self.state()["jobs"][0]
            # Assertions belong outside the callback: step intentionally catches
            # observer exceptions, including AssertionError, to defer safely.
            reads.append((current["polls"], current.get("reserved_bytes", 0),
                          current.get("reserved_records", 0), kwargs.get("limits")))
            after_read = True
            return {"status": "deferred", "reason": "source_turn_not_closed",
                    "work": {"bytes_read": 10, "records_parsed": 1}}
        def update(*args, **kwargs):
            return (False, None) if after_read else original_update(*args, **kwargs)
        with patch("recording_jobs.observe_source_turn", side_effect=observe), \
             patch("recording_jobs._update", side_effect=update):
            for _ in range(jobs.MAX_POLLS + 2):
                after_read = False
                self.step()
        self.assertEqual(len(reads), jobs.MAX_POLLS)
        for index, (polls, reserved_bytes, reserved_records, limits) in enumerate(reads, 1):
            self.assertEqual(polls, index)
            self.assertEqual(reserved_bytes, index * charge)
            self.assertEqual(reserved_records, index * jobs.Limits().events)
            self.assertEqual(limits, jobs.Limits())
        self.identity.assert_not_called()
        self.assertEqual(self.reservations, [])

    def test_large_valid_anchor_is_charged_before_insufficient_budget_refusal(self):
        self.startup()
        start = source.start("turn-1")
        start["payload"]["padding"] = "x" * (128 * 1024)
        self.append([source.header(SESSION), start, source.context("turn-1"),
                     source.user(PROMPT, identifier="prompt-turn-1", turn="turn-1")])
        self.assertEqual(jobs.notice(self.config, self.event())["outcome"], "admitted")
        self.source_complete()
        jobs.close_turn(self.config, SESSION, "turn-1")
        admission = jobs._admission(self.state()["jobs"][0]["admission"])
        charge = jobs.observation_byte_ceiling(admission)
        self.assertGreater(admission.start_record.line_bytes, 128 * 1024)
        self.assertEqual(charge, jobs.Limits().scan_bytes + 2 * (
            admission.session_record.line_bytes + admission.start_record.line_bytes))
        # More than the ordinary anchor allowance remains, but not this poll's.
        self.mutate(lambda d: d["jobs"][0].update(reserved_bytes=jobs.MAX_SCAN_WORK - charge + 1))
        with patch("recording_jobs.observe_source_turn") as observe:
            _, runtime = self.step()
        observe.assert_not_called()
        self.assertEqual(runtime.calls, [])
        self.identity.assert_not_called()
        self.assertEqual(self.state()["receipts"][-1]["reason"], "observation_work_cap")
        self.assertEqual(self.reservations, [])

    def test_invalid_anchor_cannot_get_a_cheap_reservation_or_source_read(self):
        self.closed()
        self.mutate(lambda d: d["jobs"][0]["admission"]["start_record"].update(line_bytes=-1))
        with patch("recording_jobs.observe_source_turn") as observe:
            _, runtime = self.step()
        observe.assert_not_called()
        self.assertEqual(runtime.calls, [])
        self.identity.assert_not_called()
        self.assertEqual(self.state()["receipts"][-1]["reason"], "worker_failure")
        self.assertEqual(self.reservations, [])

    def test_observer_exceeding_its_reservation_cannot_reach_assessment(self):
        for excess in ("bytes_read", "records_parsed"):
            with self.subTest(excess=excess):
                self.closed()
                charge = jobs.observation_byte_ceiling(jobs._admission(self.state()["jobs"][0]["admission"]))
                work = {"bytes_read": 1, "records_parsed": 1}
                work[excess] = (charge if excess == "bytes_read" else jobs.Limits().events) + 1
                # Below the whole-job cap: specifically the one-poll contract failed.
                self.assertLess(work["bytes_read"], jobs.MAX_SCAN_WORK)
                self.assertLess(work["records_parsed"], jobs.MAX_EVENT_WORK)
                result = {"status": "deferred", "reason": "source_turn_not_closed", "work": work}
                with patch("recording_jobs.observe_source_turn", return_value=result):
                    _, runtime = self.step()
                self.assertEqual(runtime.calls, [])
                self.identity.assert_not_called()
                self.assertEqual(self.state()["receipts"][-1]["reason"], "observation_work_cap")
                self.assertEqual(self.reservations, [])
                self.assertEqual(self.writes, [])
                jobs._path(self.config, SESSION).unlink()
                self.path().unlink()

    def test_drain_lock_is_nonblocking_and_surfaces_uncertainty(self):
        self.begin()
        lock_path = jobs._directory(self.config) / ".lock"
        with lock_path.open("r+") as stream:
            fcntl.flock(stream.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            started = REAL_MONOTONIC()
            result = jobs.drain_status(self.config, [SESSION])
            elapsed = REAL_MONOTONIC() - started
        self.assertLess(elapsed, 0.5)
        self.assertTrue(result["truncated"])
        self.assertTrue(result["usage_unknown"])
        result = jobs.drain_status(self.config, [SESSION])
        self.assertEqual(result["pending"], 1)
        self.assertEqual(result["unresolved"], 0)

    def test_changed_boot_witness_or_monotonic_expiry_cannot_renew_deadline(self):
        self.closed()
        original = self.state()["jobs"][0]
        self.boot = "a-different-boot"
        _, runtime = self.step()
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.state()["counts"]["deferred"], 1)
        self.assertEqual(self.state()["jobs"], [])
        jobs._path(self.config, SESSION).unlink()
        self.path().unlink()
        self.boot = "synthetic-boot-witness"
        self.closed()
        wall_deadline = self.state()["jobs"][0]["deadline"]
        self.now += 1
        self.monotonic = original["monotonic_deadline"] + 1
        _, runtime = self.step()
        self.assertLess(self.now, wall_deadline)
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.state()["jobs"], [])
        self.assertEqual(self.writes, [])

    def test_missing_boot_witness_defers_without_starting_a_renewable_job(self):
        self.begin()
        self.boot = None
        jobs.close_turn(self.config, SESSION)
        self.assertEqual(self.state()["jobs"], [])
        self.assertEqual(self.state()["counts"]["deferred"], 1)
        self.assertFalse(jobs.has_work(self.config, SESSION))

    def test_config_changed_between_identity_and_overlap_prevents_assessment(self):
        self.closed()
        def identity(*args, **kwargs):
            self.service.write_text('{"different_original_configuration":true}')
            return {"outcome": "ok", "db_id": DB_ID, "native_work": {"decoded_bytes": 100}}
        self.identity.side_effect = identity
        _, runtime = self.step()
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.reservations, [])
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["counts"]["deferred"], 1)

    def test_hook_config_bytes_changed_during_overlap_prevent_assessment(self):
        hook_config = self.root / "synthetic-hook-config.json"
        hook_config.write_bytes(b'{"recording_mode":"automatic"}')
        self.config["_config_path"] = hook_config
        self.closed()
        def overlap(*args, **kwargs):
            hook_config.write_bytes(b'{"recording_mode":"off"}')
            return {"outcome": "empty", "cards": [], "db_id": DB_ID}
        self.overlap.side_effect = overlap
        _, runtime = self.step()
        self.overlap.assert_called_once()
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.reservations, [])
        self.assertEqual(self.accounts, [])
        self.assertEqual(self.writes, [])
        state = self.state()
        self.assertEqual(state["jobs"], [])
        self.assertEqual(state["counts"]["deferred"], 1)
        self.assertEqual(state["receipts"][-1]["reason"], "deadline_or_config_changed")

    def test_identity_and_overlap_share_effort_native_deadline(self):
        self.closed()
        def identity(*args, **kwargs):
            self.monotonic += 1.25
            return {"outcome": "ok", "db_id": DB_ID, "native_work": {"decoded_bytes": 100}}
        self.identity.side_effect = identity
        self.step()
        first_budget = self.identity.call_args.kwargs["timeout"]
        remaining_budget = self.overlap.call_args.kwargs["timeout"]
        self.assertLessEqual(first_budget, 2)
        self.assertGreater(remaining_budget, 0)
        self.assertAlmostEqual(remaining_budget, resolve(self.config).native_seconds - 1.25)
        self.assertEqual(self.overlap.call_args.kwargs["budget"].effort, "medium")
        self.assertEqual(self.overlap.call_args.kwargs["spent_native_bytes"], 100)
        self.assertEqual(len(self.writes), 1)

    def test_identity_missing_or_invalid_byte_evidence_refuses_overlap_and_provider(self):
        for index, work in enumerate((None, {}, {"decoded_bytes": True}, {"decoded_bytes": -1},
                                     {"decoded_bytes": "100"})):
            with self.subTest(work=work):
                turn="turn-"+str(index+1)
                self.begin(turn);self.source_complete(turn)
                jobs.close_turn(self.config,SESSION,turn)
                identity={"outcome":"ok", "db_id":DB_ID}
                if work is not None: identity["native_work"]=work
                self.identity.return_value=identity
                _, runtime=self.step()
                self.overlap.assert_not_called()
                self.assertEqual(runtime.calls, [])
                self.assertEqual(self.reservations, [])
                self.assertEqual(self.accounts, [])
                self.assertEqual(self.writes, [])
                state=self.state()
                self.assertEqual(state["receipts"][-1]["reason"], "native_identity_unavailable")

    def test_identity_consuming_native_allowance_does_not_start_discovery(self):
        from librarian_policy import resolve
        self.closed()
        self.identity.return_value={"outcome":"ok", "db_id":DB_ID,
                                    "native_work":{"decoded_bytes":resolve(self.config).native_read_bytes}}
        _, runtime=self.step()
        self.overlap.assert_not_called()
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.reservations, [])
        self.assertEqual(self.state()["receipts"][-1]["reason"], "overlap_unavailable_or_changed")

    def test_expired_native_phase_cannot_start_provider_work(self):
        self.closed()
        native_started = self.monotonic
        def overlap(*args, **kwargs):
            self.monotonic += resolve(self.config).native_seconds + .1
            return {"outcome": "empty", "cards": [], "db_id": DB_ID}
        self.overlap.side_effect = overlap
        _, runtime = self.step()
        self.overlap.assert_called_once()
        self.assertGreater(self.monotonic - native_started, resolve(self.config).native_seconds)
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.reservations, [])
        self.assertEqual(self.writes, [])

    def test_export_preserves_bounded_verified_source_and_native_receipt_not_locator(self):
        self.closed()
        _, runtime = self.step()
        result = jobs.export_receipts(self.config, [SESSION])
        self.assertFalse(result["unknown"])
        self.assertFalse(result["truncated"])
        self.assertEqual(len(result["receipts"]), 1)
        receipt = result["receipts"][0]
        self.assertEqual(receipt["source"]["session_id"], SESSION)
        self.assertEqual(receipt["source"]["turn_id"], "turn-1")
        self.assertEqual(receipt["source"]["prompt_sha256"], jobs.digest(PROMPT.encode()))
        self.assertIn("raw_line_sha256", receipt["source"]["start_ref"])
        self.assertIn("raw_line_sha256", receipt["closure_ref"])
        self.assertEqual(receipt["proposal"]["kind"], "lesson")
        _, context = contract.prepare(runtime.calls[0][0], runtime.calls[0][1])
        binding = context.bindings[0]
        self.assertEqual(receipt["citations"], [{
            "evidence_id": binding.evidence_id, "kind": binding.kind,
            "source_ref_json": binding.source_ref_json, "source_field": binding.source_field,
            "rendering": binding.rendering}])
        self.assertNotIn(PROMPT.encode(), jobs.encoded(receipt["citations"]))
        self.assertEqual(receipt["native"]["db_id"], DB_ID)
        self.assertEqual(receipt["native"]["readback_status"], "verified")
        self.assertEqual(receipt["observation"], {
            "source_turn": "closed_verified", "mode": "all", "observed_records": 2,
            "selected_records": 2, "omitted_records": 0})
        serialized = jobs.encoded(result)
        self.assertNotIn(str(self.root).encode(), serialized)
        for excluded in (b'"path"', b'"sessions_root"', b'"admission"'):
            self.assertNotIn(excluded, serialized)
        self.assertLessEqual(len(serialized), jobs.MAX_RECEIPTS_BYTES)
        self.assertLessEqual(len(jobs.encoded(receipt)), jobs.MAX_RECEIPT_BYTES)

    def test_long_closed_turn_preflight_and_mock_assessment_share_selected_input(self):
        self.begin()
        self.append([source.assistant("old-" + str(index) + ":" + "x" * 20000,
                                      identifier=f"comment-{index}", turn="turn-1",
                                      phase="commentary")
                     for index in range(8)])
        self.append([source.assistant("Latest is not verified success.", identifier="last-final",
                                      turn="turn-1"), source.event("task_complete", "turn-1")])
        jobs.close_turn(self.config, SESSION, "turn-1")
        _, runtime = self.step()
        self.assertEqual(len(runtime.calls), 1)
        observed, overlap, _ = runtime.calls[0]
        self.assertEqual(observed["status"], "complete")
        first_prompt, first_context = contract.prepare(observed, overlap)
        second_prompt, second_context = contract.prepare(observed, overlap)
        self.assertEqual((first_prompt, first_context), (second_prompt, second_context))
        selected = json.loads(first_context.coverage_json)["public_evidence"]
        self.assertEqual(selected["mode"], "selected_suffix")
        self.assertEqual(selected["observed_records"], 10)
        self.assertLess(selected["selected_records"], 10)
        receipt = jobs.export_receipts(self.config, [SESSION])["receipts"][0]
        self.assertEqual(receipt["observation"], {"source_turn": "closed_verified", **selected})
        self.assertEqual(receipt["citations"][0]["evidence_id"], first_context.bindings[0].evidence_id)
        self.assertNotIn("old-0:", jobs.encoded(receipt).decode())
        self.assertEqual(len(self.writes), 1)

    def test_selected_source_citations_export_host_metadata_not_source_text_or_native_payload(self):
        admitted = self.begin()
        call_text = "SYNTHETIC_PRIVATE_CALL_INPUT"
        result_text = "SYNTHETIC_PRIVATE_TOOL_RESULT"
        assistant_text = "SYNTHETIC_PRIVATE_ASSISTANT_TEXT"
        self.append([source.call(turn="turn-1", text=call_text),
                     source.output(turn="turn-1", text=result_text),
                     source.assistant(assistant_text, turn="turn-1"),
                     source.event("task_complete", "turn-1")])
        jobs.close_turn(self.config, SESSION, "turn-1")
        runtime = FakeRuntime(self)
        original = runtime.assess
        expected = []
        def assess(observation, overlap, *, timeout):
            result = original(observation, overlap, timeout=timeout)
            _, context = contract.prepare(observation, overlap)
            selected = {binding.kind: binding for binding in context.bindings}
            bindings = [selected[kind] for kind in (
                "user_statement", "tool_call", "tool_result", "assistant_assertion")]
            expected.extend({"evidence_id": binding.evidence_id, "kind": binding.kind,
                             "source_ref_json": binding.source_ref_json,
                             "source_field": binding.source_field, "rendering": binding.rendering}
                            for binding in bindings)
            result["proposal"] = contract.validate_answer({"proposal": {
                "kind": "lesson", "summary": "User chose R5.",
                "body": "The user corrected the target to R5.",
                "evidence_ids": [binding.evidence_id for binding in bindings],
                "associate_with": None}}, context)["proposal"]
            return result
        runtime.assess = assess
        self.step(runtime)
        self.assertEqual(len(expected), 4)
        self.assertEqual((len(runtime.calls), len(self.accounts), len(self.writes)), (1, 1, 1))
        exported = jobs.export_receipts(self.config, [SESSION])
        self.assertFalse(exported["unknown"])
        self.assertFalse(exported["truncated"])
        self.assertEqual(exported["receipts"][0]["citations"], expected)
        self.assertEqual(self.writes[0][1]["citations"], expected)
        payload = self.writes[0][1]["payload"]
        self.assertEqual(payload, {
            "source": {"namespace": "codex-acquisition.v1", "key": admitted["key"],
                       "reference": f"codex://{SESSION}/turn-1", "session": SESSION},
            "summary": "User chose R5.", "body": "The user corrected the target to R5."})
        for citation in exported["receipts"][0]["citations"]:
            self.assertEqual(set(citation), {"evidence_id", "kind", "source_ref_json", "source_field", "rendering"})
        for text in (PROMPT, call_text, result_text, assistant_text):
            self.assertNotIn(text.encode(), jobs.encoded(exported))
            self.assertNotIn(text.encode(), jobs.encoded(payload))
        self.assertFalse(self.step(runtime)[0])
        self.assertEqual((len(runtime.calls), len(self.accounts), len(self.writes)), (1, 1, 1))

    def test_export_null_receipt_has_source_and_closure_but_no_invented_proposal(self):
        self.closed()
        self.step(FakeRuntime(self, reason="abstained"))
        result = jobs.export_receipts(self.config, [SESSION])
        receipt = result["receipts"][0]
        self.assertEqual(receipt["outcome"], "abstained")
        self.assertIn("source", receipt)
        self.assertIn("closure_ref", receipt)
        self.assertNotIn("proposal", receipt)
        self.assertNotIn("native", receipt)
        self.assertFalse(result["unknown"])

    def test_export_global_record_cap_is_not_per_ledger(self):
        sessions = ["export-one", "export-two"]
        for session in sessions:
            self.startup(session)
            receipts = [{"source_key": f"{session}-{i}", "outcome": "abstained", "reason": "synthetic"}
                        for i in range(jobs.MAX_RECEIPTS)]
            self.mutate(lambda data: data.update(receipts=receipts), session)
        result = jobs.export_receipts(self.config, sessions)
        self.assertEqual(len(result["receipts"]), jobs.MAX_RECEIPTS)
        self.assertTrue(result["truncated"])
        self.assertFalse(result["unknown"])
        self.assertLessEqual(len(jobs.encoded(result)), jobs.MAX_RECEIPTS_BYTES)

    def test_export_per_record_and_total_byte_bounds_are_enforced(self):
        self.startup()
        oversized = {"source_key": "oversized", "outcome": "abstained", "extra": "x" * 9000}
        self.mutate(lambda data: data.update(receipts=[oversized]))
        result = jobs.export_receipts(self.config, [SESSION])
        self.assertEqual(result["receipts"], [])
        self.assertTrue(result["truncated"])
        near_limit = []
        for i in range(jobs.MAX_RECEIPTS):
            item = {"source_key": str(i), "outcome": "abstained", "extra": ""}
            overhead = len(jobs.encoded({"session_id": SESSION, **item}))
            item["extra"] = "x" * (jobs.MAX_RECEIPT_BYTES - overhead)
            near_limit.append(item)
        self.mutate(lambda data: data.update(receipts=near_limit))
        result = jobs.export_receipts(self.config, [SESSION])
        self.assertTrue(result["truncated"])
        self.assertLess(len(result["receipts"]), jobs.MAX_RECEIPTS)
        self.assertLessEqual(len(jobs.encoded(result)), jobs.MAX_RECEIPTS_BYTES)
        self.assertTrue(all(len(jobs.encoded(item)) <= jobs.MAX_RECEIPT_BYTES for item in result["receipts"]))

    def test_export_retained_omissions_and_missing_ledger_are_not_complete_history(self):
        self.startup()
        self.mutate(lambda data: data.update(receipt_omissions=2))
        result = jobs.export_receipts(self.config, [SESSION])
        self.assertTrue(result["truncated"])
        self.assertFalse(result["unknown"])
        jobs._path(self.config, SESSION).unlink()
        missing = jobs.export_receipts(self.config, [SESSION])
        self.assertEqual(missing["receipts"], [])
        self.assertTrue(missing["unknown"])
        too_many = jobs.export_receipts(self.config, [f"session-{i}" for i in range(jobs.MAX_LEDGERS + 1)])
        self.assertTrue(too_many["truncated"])
        self.assertTrue(too_many["unknown"])

    def test_export_unresolved_remains_private_and_reports_unknown(self):
        self.closed()
        self.mutate(lambda data: data["jobs"][0].update(phase="unresolved", receipt={"accepted": True}))
        result = jobs.export_receipts(self.config, [SESSION])
        self.assertTrue(result["unknown"])
        self.assertEqual(result["receipts"], [])
        self.assertNotIn(str(self.root), json.dumps(result))
        self.assertTrue(self.state()["jobs"][0]["receipt"]["accepted"])

    def test_export_held_lock_is_bounded_and_reports_unknown(self):
        self.startup()
        lock_path = jobs._directory(self.config) / ".lock"
        with lock_path.open("r+") as stream:
            fcntl.flock(stream.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            started = REAL_MONOTONIC()
            result = jobs.export_receipts(self.config, [SESSION])
            elapsed = REAL_MONOTONIC() - started
        self.assertLess(elapsed, 0.5)
        self.assertTrue(result["unknown"])
        self.assertEqual(result["receipts"], [])

    def test_disabled_mode_never_creates_a_ledger(self):
        self.config["recording_mode"] = "off"
        self.source_start()
        self.startup()
        jobs.notice(self.config, self.event())
        jobs.close_turn(self.config, SESSION)
        self.assertFalse(jobs._directory(self.config).exists())

    def test_context_normalizer_keeps_core_and_old_absence_unknown(self):
        core={'outcome':'verified','native':{'id':OTHER_DB},'source':{'hash':'unchanged'}}
        self.assertEqual(jobs.normalize_context_diagnostic_fields(core),core)
        valid={'observer_memory_delivery':'admitted_retained','prompt_memory_delivery':'admitted_retained',
               'shown_semantic_targets':1,'shown_routing_targets':0,'routing_enabled':False}
        for invalid in ('PRIVATE'*10000, {'raw_prompt':'PRIVATE'},
                        {**valid,'shown_semantic_targets':True}, {**valid,'routing_enabled':'yes'},
                        {**valid,'prompt_memory_delivery':'PRIVATE'*10000}):
            row={**core,'context_diagnostic':invalid}
            result=jobs.normalize_context_diagnostic_fields(row)
            self.assertEqual({key:result[key] for key in core},core)
            self.assertTrue(result['context_diagnostic_omitted'])
            self.assertEqual(result['context_diagnostic'],jobs.sanitize_context_diagnostic(None))
            self.assertNotIn('PRIVATE',jobs.encoded(result).decode())
        row={**core,'context_diagnostic':valid}
        original=copy.deepcopy(row)
        self.assertEqual(jobs.normalize_context_diagnostic_fields(row),row)
        self.assertLessEqual(len(jobs.encoded(valid)),jobs.MAX_CONTEXT_DIAGNOSTIC_BYTES)
        exact=jobs.normalize_context_diagnostic_fields(row,max_bytes=len(jobs.encoded(core)))
        self.assertEqual(exact,core)
        self.assertEqual(row,original)
        marked=jobs.normalize_context_diagnostic_fields(row,force_omit=True)
        self.assertTrue(marked['context_diagnostic_omitted'])
        self.assertNotIn('context_diagnostic',marked)

    def test_real_observer_marker_omission_and_no_final_targets_are_distinct(self):
        self.begin()
        packet=self.delivery_packet()
        jobs.attach_delivery(self.config,packet)
        self.append([source.call(turn='turn-1'),source.delivered(packet,turn='turn-1'),
                     source.output(turn='turn-1')])
        self.source_complete();jobs.close_turn(self.config,SESSION,'turn-1')
        # Mandatory user/final consume both slots; even the atomic marker cannot fit.
        limits=jobs.Limits(items=2)
        with patch.object(jobs,'Limits',return_value=limits):
            _,runtime=self.step(FakeRuntime(self,reason='abstained'))
        self.assertEqual(runtime.calls[0][0]['coverage']['memory_delivery'],'admitted_omitted')
        receipt=jobs.export_receipts(self.config,[SESSION])['receipts'][0]
        self.assertEqual(receipt['context_diagnostic'],{
            'observer_memory_delivery':'admitted_omitted','prompt_memory_delivery':'admitted_omitted',
            'shown_semantic_targets':0,'shown_routing_targets':0,'routing_enabled':False})
        self.assertEqual((len(runtime.calls),len(self.reservations),len(self.accounts),len(self.writes)),(1,1,1,0))
        self.assertNotIn(packet['rendered_text'],jobs.encoded(receipt).decode())

    def test_final_prompt_packing_removes_observer_retained_marker_and_targets(self):
        from test_recording_contract import observation,statement,memory_marker,tool_call,tool_result
        items=[statement('Inspect q7.'),memory_marker(),tool_call(ordinal=3),
               tool_result(ordinal=4,output='x'*5000),
               statement('Reported checks.',kind='assistant_assertion',ordinal=5)]
        # Anchor-only authored room cannot fit the marker, unlike the older
        # optional-tool-pair ceiling that the new reservation intentionally beats.
        _,baseline=contract.prepare(observation([items[0],items[-1]]))
        observed=observation(items)
        with patch.object(contract,'MAX_AUTHORED_BYTES',baseline.authored_bytes+100):
            prompt,context=contract.prepare(observed)
        self.assertIsNotNone(prompt)
        value=jobs._context_diagnostic(observed,context)
        self.assertEqual(value,{'observer_memory_delivery':'admitted_retained',
            'prompt_memory_delivery':'admitted_omitted','shown_semantic_targets':0,
            'shown_routing_targets':0,'routing_enabled':False})
        self.assertEqual(context.association_bindings,())

    def test_long_turn_delivery_anchor_reaches_assessor_without_reviving_omitted_outcome(self):
        self.begin()
        packet = self.delivery_packet()
        jobs.attach_delivery(self.config, packet)
        self.append([source.call('early', turn='turn-1'),
                     source.delivered(packet, turn='turn-1'),
                     source.output('early', turn='turn-1', text='OMITTED result ' + 'x' * 40000),
                     source.call('later', turn='turn-1'),
                     source.output('later', turn='turn-1', text='Current constraint: R5 required.')])
        self.source_complete()
        jobs.close_turn(self.config, SESSION, 'turn-1')
        _, runtime = self.step(FakeRuntime(self, reason='abstained'))
        observed = runtime.calls[0][0]
        self.assertEqual([item['kind'] for item in observed['evidence']],
                         ['user_statement', 'memory_delivery', 'tool_call', 'tool_result',
                          'assistant_assertion'])
        prompt, context = contract.prepare(observed)
        self.assertIsInstance(context, contract.ValidationContext)
        self.assertNotIn('OMITTED result', prompt)
        self.assertIn('Current constraint: R5 required.', prompt)
        self.assertEqual(context.association_bindings[0].full_get_fingerprint, 'a' * 64)
        receipt = jobs.export_receipts(self.config, [SESSION])['receipts'][0]
        self.assertEqual(receipt['outcome'], 'abstained')
        self.assertEqual(receipt['context_diagnostic'], {
            'observer_memory_delivery': 'admitted_retained', 'prompt_memory_delivery': 'admitted_retained',
            'shown_semantic_targets': 1, 'shown_routing_targets': 0, 'routing_enabled': False})
        self.assertEqual((len(runtime.calls), len(self.reservations), len(self.accounts), len(self.writes)),
                         (1, 1, 1, 0))
        self.assertEqual(self.accounts[0][1]['usage'], {'input_tokens': 20, 'output_tokens': 5})

    def test_bound_routing_targets_are_not_a_usefulness_judgment(self):
        self.routed_closed()
        _,runtime=self.step(FakeRuntime(self,reason='abstained'))
        receipt=jobs.export_receipts(self.config,[SESSION])['receipts'][0]
        self.assertEqual(receipt['outcome'],'abstained')
        self.assertEqual(receipt['context_diagnostic'],{
            'observer_memory_delivery':'admitted_retained','prompt_memory_delivery':'admitted_retained',
            'shown_semantic_targets':1,'shown_routing_targets':1,'routing_enabled':True})
        self.assertEqual((len(runtime.calls),len(self.accounts),len(self.writes)),(1,1,0))
        self.assertNotIn('useful',jobs.encoded(receipt).decode())

    def test_bound_routing_targets_do_not_imply_prepared_routing_is_enabled(self):
        self.routed_closed()
        job=self.state()['jobs'][0]
        observed=jobs.observe_source_turn(jobs._admission(job['admission']),delivery=job['delivery'])
        _,baseline=contract.prepare(observed,[])
        self.assertTrue(baseline.routing_enabled)
        with patch.object(contract,'MAX_AUTHORED_BYTES',baseline.authored_bytes-1):
            prompt,context=contract.prepare(observed,[])
        self.assertIsNotNone(prompt)
        value=jobs._context_diagnostic(observed,context)
        self.assertEqual(value['prompt_memory_delivery'],'admitted_retained')
        self.assertEqual((value['shown_semantic_targets'],value['shown_routing_targets'],value['routing_enabled']),
                         (1,1,False))
        self.assertEqual((self.reservations,self.accounts,self.writes),([],[],[]))

    def test_preparation_refusal_keeps_observer_state_and_existing_sanitized_reason(self):
        self.closed()
        for reason,expected in [('authored_input_limit','authored_input_limit')]:
            with patch.object(contract,'prepare',return_value=(None,reason)):
                _,runtime=self.step(FakeRuntime(self))
        receipt=jobs.export_receipts(self.config,[SESSION])['receipts'][0]
        self.assertEqual((receipt['outcome'],receipt['reason']),('deferred','assessment_input_refused'))
        self.assertEqual(receipt['assessment_diagnostic']['runtime_reason'],expected)
        self.assertEqual(receipt['context_diagnostic'],{
            'observer_memory_delivery':'not_recorded','prompt_memory_delivery':'unknown',
            'shown_semantic_targets':None,'shown_routing_targets':None,'routing_enabled':None})
        self.assertEqual((runtime.calls,self.reservations,self.accounts,self.writes),([],[],[],[]))

    def test_preparation_refusal_raw_reason_is_unknown_not_retained(self):
        self.closed()
        with patch.object(contract,'prepare',return_value=(None,'PRIVATE prompt/provider details')):
            _,runtime=self.step(FakeRuntime(self))
        receipt=jobs.export_receipts(self.config,[SESSION])['receipts'][0]
        self.assertEqual(receipt['assessment_diagnostic'],{'runtime_reason':'unknown','validation_reason':'unknown'})
        self.assertNotIn('PRIVATE',jobs.encoded(receipt).decode())
        self.assertEqual((runtime.calls,self.reservations,self.accounts,self.writes),([],[],[],[]))

    def test_context_shed_before_exact_job_and_global_core_state_caps(self):
        self.closed()
        original=self.state();context=jobs.sanitize_context_diagnostic(None)
        data=copy.deepcopy(original);job=data['jobs'][0]
        job.update(phase='unresolved',reason='usage_unknown',padding='')
        job['padding']='x'*(jobs.MAX_JOB_BYTES-len(jobs.encoded(job)))
        core=copy.deepcopy(data)
        job['context_diagnostic']=context
        jobs._write(jobs._path(self.config,SESSION),data)
        self.assertEqual(self.state(),core)
        aggregate=copy.deepcopy(original)
        jobs._finish(aggregate,aggregate['jobs'][0],'deferred','assessment_refused')
        aggregate['jobs']=copy.deepcopy(original['jobs'])
        core=copy.deepcopy(aggregate)
        for row in aggregate['jobs']+aggregate['receipts']:row['context_diagnostic']=context
        with patch.object(jobs,'MAX_STATE',len(jobs.encoded(core))):
            jobs._write(jobs._path(self.config,SESSION),aggregate)
            self.assertEqual(self.state(),core)
        self.assertEqual((self.reservations,self.accounts,self.writes),([],[],[]))

    def test_context_shed_preserves_unresolved_core_at_768_byte_boundary(self):
        core={'session_id':'s'*160,'source_key':'a'*64,'phase':'unresolved',
              'reason':'accounting_unavailable','ledger_usage_unknown':True,
              'assessment_diagnostic':{'runtime_reason':'unsupported_public_selection','validation_reason':'unknown'}}
        row={**core,'context_diagnostic':jobs.sanitize_context_diagnostic(None)}
        exact=len(jobs.encoded(core))
        with patch.object(jobs,'MAX_UNRESOLVED_DIAGNOSTIC_BYTES',exact):
            value=jobs.sanitize_unresolved_diagnostic(row)
        self.assertEqual(value,core)
        self.assertIsNotNone(jobs.sanitize_unresolved_diagnostic(row))

    def test_unresolved_bucket_shed_context_marks_optional_omission(self):
        core={'session_id':SESSION,'source_key':'a'*64,'phase':'unresolved',
              'reason':'accounting_unavailable','ledger_usage_unknown':True,
              'assessment_diagnostic':{'runtime_reason':'usage_unknown','validation_reason':'unknown'}}
        row={**core,'context_diagnostic':jobs.sanitize_context_diagnostic(None)}
        envelope={'schema':'mneme.codex-recording.receipts.v1','receipts':[],
                  'unknown':True,'truncated':False}
        with patch.object(jobs,'MAX_UNRESOLVED_DIAGNOSTICS_BYTES',len(jobs.encoded([core]))):
            value=jobs.normalize_unresolved_export(envelope,[row])
        self.assertEqual(value['unresolved_diagnostics'],[core])
        self.assertTrue(value['unresolved_diagnostics_omitted'])
        self.assertEqual({key:value[key] for key in envelope},envelope)

    def test_unresolved_pressure_sheds_prior_context_before_dropping_later_core(self):
        core={'session_id':SESSION,'source_key':'a'*64,'phase':'unresolved',
              'reason':'accounting_unavailable','ledger_usage_unknown':True,
              'assessment_diagnostic':{'runtime_reason':'usage_unknown','validation_reason':'unknown'}}
        later={**core,'source_key':'b'*64}
        row={**core,'context_diagnostic':jobs.sanitize_context_diagnostic(None)}
        envelope={'schema':'mneme.codex-recording.receipts.v1','receipts':[],
                  'unknown':True,'truncated':False}
        with patch.object(jobs,'MAX_UNRESOLVED_DIAGNOSTICS_BYTES',len(jobs.encoded([core,later]))):
            value=jobs.normalize_unresolved_export(envelope,[row,later])
        self.assertEqual(value['unresolved_diagnostics'],[core,later])
        self.assertTrue(value['unresolved_diagnostics_omitted'])

    def test_unresolved_export_sheds_receipt_context_before_losing_unresolved_core(self):
        core={'session_id':SESSION,'source_key':'a'*64,'phase':'unresolved',
              'reason':'accounting_unavailable','ledger_usage_unknown':True,
              'assessment_diagnostic':{'runtime_reason':'usage_unknown','validation_reason':'unknown'}}
        receipt={'outcome':'verified','native':{'id':OTHER_DB}}
        envelope={'schema':'mneme.codex-recording.receipts.v1',
                  'receipts':[{**receipt,'context_diagnostic':jobs.sanitize_context_diagnostic(None)}],
                  'unknown':True,'truncated':False}
        before=copy.deepcopy(envelope)
        expected={**envelope,'receipts':[receipt],'unresolved_diagnostics':[core],
                  'unresolved_diagnostics_omitted':True}
        with patch.object(jobs,'MAX_RECEIPTS_BYTES',len(jobs.encoded(expected))):
            value=jobs.normalize_unresolved_export(envelope,[core])
        self.assertEqual(value,expected)
        self.assertEqual(envelope,before)

    def test_context_receipt_and_export_pressure_preserves_source_and_native_core(self):
        self.closed();self.step(FakeRuntime(self,reason='abstained'))
        before=self.state()
        normal=jobs.export_receipts(self.config,[SESSION])
        core=copy.deepcopy(normal)
        core['receipts'][0].pop('context_diagnostic')
        with patch.object(jobs,'MAX_RECEIPT_BYTES',len(jobs.encoded(core['receipts'][0]))):
            value=jobs.export_receipts(self.config,[SESSION])
        self.assertEqual(value,core)
        with patch.object(jobs,'MAX_RECEIPTS_BYTES',len(jobs.encoded(core))):
            value=jobs.export_receipts(self.config,[SESSION])
        self.assertEqual(value,core)
        self.assertFalse(value['unknown']);self.assertFalse(value['truncated'])
        self.assertEqual(self.state(),before)

    def test_known_cost_refusal_retains_only_allowlisted_diagnostic_without_retry(self):
        self.closed()
        runtime = FakeRuntime(self, reason="invalid_output")
        original = runtime.assess
        def assess(*args, **kwargs):
            return {**original(*args, **kwargs), "validation_reason": "invalid_exact_quote",
                    "raw_answer": "NEVER RETAIN THIS RESPONSE", "exception": "private details"}
        runtime.assess = assess
        self.step(runtime)
        receipt = self.state()["receipts"][-1]
        self.assertEqual((receipt["outcome"], receipt["reason"]), ("deferred", "assessment_refused"))
        self.assertEqual(receipt["assessment_diagnostic"], {
            "runtime_reason": "invalid_output", "validation_reason": "invalid_exact_quote"})
        self.assertEqual(self.accounts[0][1]["usage"], {"input_tokens": 20, "output_tokens": 5})
        self.assertEqual(len(self.accounts), 1)
        self.assertEqual(self.writes, [])
        self.assertFalse(self.step(runtime)[0])
        self.assertEqual(len(runtime.calls), 1)
        encoded = jobs.encoded(jobs.export_receipts(self.config, [SESSION]))
        self.assertNotIn(b"NEVER RETAIN", encoded)
        self.assertNotIn(b"private details", encoded)

    def test_unknown_runtime_and_validation_codes_never_escape_or_change_refusal(self):
        self.closed()
        runtime = FakeRuntime(self, reason="arbitrary model-authored reason / secret")
        original = runtime.assess
        runtime.assess = lambda *a, **k: {**original(*a, **k), "validation_reason": {"secret": "x" * 10000}}
        self.step(runtime)
        receipt = self.state()["receipts"][-1]
        self.assertEqual(receipt["reason"], "assessment_refused")
        self.assertEqual(receipt["assessment_diagnostic"], {
            "runtime_reason": "unknown", "validation_reason": "unknown"})
        self.assertEqual((len(self.accounts), len(self.writes)), (1, 0))

    def test_null_diagnostic_stays_abstention_and_never_validates_untrusted_category(self):
        self.closed()
        runtime = FakeRuntime(self, reason="abstained")
        original = runtime.assess
        runtime.assess = lambda *a, **k: {**original(*a, **k), "validation_reason": "invalid_exact_quote"}
        self.step(runtime)
        receipt = self.state()["receipts"][-1]
        self.assertEqual(receipt["outcome"], "abstained")
        self.assertEqual(receipt["assessment_diagnostic"], {
            "runtime_reason": "abstained", "validation_reason": "unknown"})
        self.assertNotIn("proposal", receipt)
        self.assertEqual((len(self.accounts), len(self.writes)), (1, 0))

    def test_unknown_usage_diagnostic_does_not_convert_unresolved_into_retryable_failure(self):
        self.closed()
        runtime = FakeRuntime(self, reason="timeout")
        original = runtime.assess
        runtime.assess = lambda *a, **k: {**original(*a, **k), "usage": None}
        self.step(runtime)
        job = self.state()["jobs"][0]
        self.assertEqual((job["phase"], job["reason"]), ("unresolved", "usage_unknown"))
        self.assertEqual(job["assessment_diagnostic"], {
            "runtime_reason": "timeout", "validation_reason": "unknown"})
        self.assertTrue(self.state()["usage_unknown"])
        self.assertFalse(self.step(runtime)[0])
        self.assertEqual((len(runtime.calls), len(self.accounts), len(self.writes)), (1, 1, 0))
        before = self.state()
        exported = jobs.export_receipts(self.config, [SESSION])
        self.assertEqual(self.state(), before)
        self.assertTrue(exported["unknown"])
        self.assertEqual(exported["receipts"], [])
        self.assertFalse(exported["unresolved_diagnostics_omitted"])
        row = exported["unresolved_diagnostics"][0]
        self.assertEqual(row, {"session_id": SESSION, "source_key": job["key"],
            "phase": "unresolved", "reason": "usage_unknown", "ledger_usage_unknown": True,
            "assessment_diagnostic": {"runtime_reason": "timeout", "validation_reason": "unknown"},
            "context_diagnostic": {"observer_memory_delivery":"not_recorded", "prompt_memory_delivery":"not_recorded",
                "shown_semantic_targets":0, "shown_routing_targets":0, "routing_enabled":False}})

    def test_unresolved_export_does_not_retain_raw_job_fields_or_native_receipts(self):
        self.closed()
        self.mutate(lambda data: data["jobs"][0].update(phase="unresolved", reason="private-provider-text",
            receipt={"body": "private-native-text"}, raw_stderr="private-stderr",
            assessment_diagnostic={"raw_answer": "private-answer"}))
        result = jobs.export_receipts(self.config, [SESSION])
        row = result["unresolved_diagnostics"][0]
        self.assertEqual(row["reason"], "unknown")
        self.assertFalse(row["ledger_usage_unknown"])  # This is a ledger flag, not a recovered usage claim.
        self.assertEqual(row["assessment_diagnostic"], {"runtime_reason": "unknown", "validation_reason": "unknown"})
        self.assertNotIn("private-", json.dumps(result))
        self.assertTrue(result["unknown"])

    def test_malformed_optional_unresolved_row_is_omitted_without_losing_receipts(self):
        self.closed()
        self.mutate(lambda data: data["jobs"][0].update(phase="unresolved", key="private-not-a-hash"))
        result = jobs.export_receipts(self.config, [SESSION])
        self.assertTrue(result["unknown"])
        self.assertEqual(result["unresolved_diagnostics"], [])
        self.assertTrue(result["unresolved_diagnostics_omitted"])

    def test_optional_unresolved_projection_caps_and_never_trims_core_evidence(self):
        base = {"schema": "mneme.codex-recording.receipts.v1", "receipts": [{"body": "core"}],
                "truncated": False, "unknown": True}
        row = {"session_id": SESSION, "source_key": "a" * 64, "phase": "unresolved",
               "reason": "usage_unknown", "ledger_usage_unknown": True}
        for rows in ([row] * 17, [None, row], None):
            projected = jobs.normalize_unresolved_export(base, rows)
            self.assertEqual(projected["receipts"], base["receipts"])
            self.assertTrue(projected["unknown"])
            self.assertFalse(projected["truncated"])
            self.assertTrue(projected["unresolved_diagnostics_omitted"])
            self.assertLessEqual(len(projected["unresolved_diagnostics"]), 16)
        with patch.object(jobs, "MAX_UNRESOLVED_DIAGNOSTIC_BYTES", 1):
            projected = jobs.normalize_unresolved_export(base, [row])
            self.assertEqual(projected["unresolved_diagnostics"], [])
            self.assertTrue(projected["unresolved_diagnostics_omitted"])
        with patch.object(jobs, "MAX_UNRESOLVED_DIAGNOSTICS_BYTES", 1):
            projected = jobs.normalize_unresolved_export(base, [row])
            self.assertEqual(projected["unresolved_diagnostics"], [])
            self.assertTrue(projected["unresolved_diagnostics_omitted"])
        with patch.object(jobs, "MAX_RECEIPTS_BYTES", len(jobs.encoded(base))):
            self.assertEqual(jobs.normalize_unresolved_export(base, [row]), base)

    def test_optional_diagnostic_normalizes_before_export_bounds_and_missing_stays_absent(self):
        self.closed()
        self.step(FakeRuntime(self, reason="abstained"))
        for value in (None, "x" * 10000, {"runtime_reason": "invalid_output", "extra": "secret"},
                      {"runtime_reason": ["secret"], "validation_reason": "bad"}):
            with self.subTest(value_type=type(value).__name__):
                self.mutate(lambda data: data["receipts"][0].update(assessment_diagnostic=value))
                result = jobs.export_receipts(self.config, [SESSION])
                self.assertFalse(result["unknown"])
                self.assertFalse(result["truncated"])
                diagnostic = result["receipts"][0]["assessment_diagnostic"]
                self.assertEqual(diagnostic, {"runtime_reason": "unknown", "validation_reason": "unknown"})
                self.assertLessEqual(len(jobs.encoded(diagnostic)), 256)
                self.assertLessEqual(len(jobs.encoded(result)), jobs.MAX_RECEIPTS_BYTES)
        self.mutate(lambda data: data["receipts"][0].pop("assessment_diagnostic"))
        self.assertNotIn("assessment_diagnostic", jobs.export_receipts(self.config, [SESSION])["receipts"][0])

    def test_optional_diagnostic_drops_before_core_receipt_at_byte_boundary(self):
        self.closed()
        original = self.state()
        baseline = json.loads(json.dumps(original))
        jobs._finish(baseline, baseline["jobs"][0], "deferred", "assessment_refused")
        expected = baseline["receipts"][0]
        cap = len(jobs.encoded(expected)) + 192
        with patch.object(jobs, "MAX_RECEIPT_BYTES", cap):
            jobs._finish(original, original["jobs"][0], "deferred", "assessment_refused",
                         diagnostic={"runtime_reason": "invalid_output", "validation_reason": "invalid_exact_quote"})
        self.assertEqual(original["receipts"], [expected])
        self.assertNotIn("receipt_omissions", original)

    def test_optional_diagnostic_drops_after_unresolved_state_growth_at_32kib(self):
        self.closed()
        diagnostic = {"runtime_reason": "timeout", "validation_reason": "unknown"}
        data = self.state()
        job = data["jobs"][0]
        job["padding"] = ""
        job["padding"] = "x" * (jobs.MAX_JOB_BYTES - len(jobs.encoded({
            **job, "assessment_diagnostic": diagnostic})))
        self.assertEqual(len(jobs.encoded({**job, "assessment_diagnostic": diagnostic})), 32 * 1024)
        jobs._write(jobs._path(self.config, SESSION), data)
        jobs._finish(data, job, "unresolved", "usage_unknown", diagnostic=diagnostic)
        self.assertGreater(len(jobs.encoded(job)), jobs.MAX_JOB_BYTES)
        expected = json.loads(json.dumps(data))
        expected["jobs"][0].pop("assessment_diagnostic")
        self.assertLessEqual(len(jobs.encoded(expected["jobs"][0])), jobs.MAX_JOB_BYTES)
        jobs._write(jobs._path(self.config, SESSION), data)
        self.assertEqual(self.state(), expected)
        self.assertEqual((self.state()["jobs"][0]["phase"], self.state()["jobs"][0]["reason"]),
                         ("unresolved", "usage_unknown"))
        self.assertFalse(self.step()[0])
        self.assertEqual((self.reservations, self.accounts, self.writes), ([], [], []))

    def test_optional_diagnostic_cannot_block_frozen_to_write_intent_six_byte_growth(self):
        self.closed()
        payload = {"source": {"namespace": "codex-acquisition.v1", "key": "frozen-source"},
                   "summary": "Already frozen.", "body": "The exact native payload stays unchanged."}
        diagnostic = {"runtime_reason": "proposed", "validation_reason": "unknown"}
        data = self.state()
        job = data["jobs"][0]
        job.update(phase="frozen", db_id=DB_ID, proposal_kind="lesson", payload=payload,
                   citations=[], assessment_diagnostic=diagnostic, padding="")
        job["padding"] = "x" * (jobs.MAX_JOB_BYTES - len(jobs.encoded(job)))
        self.assertEqual(len(jobs.encoded(job)), 32 * 1024)
        self.assertEqual(len(jobs.encoded({**job, "phase": "write_intent"})), 32 * 1024 + 6)
        core = {k: v for k, v in job.items() if k != "assessment_diagnostic"}
        self.assertLessEqual(len(jobs.encoded({**core, "phase": "write_intent"})), jobs.MAX_JOB_BYTES)
        jobs._write(jobs._path(self.config, SESSION), data)
        durable_at_send = []
        def native(config, frozen, timeout):
            durable_at_send.append(self.state()["jobs"][0])
            return self.native_write(config, frozen, timeout)
        _, runtime = self.step(native_write=native)
        self.assertEqual(len(durable_at_send), 1)
        self.assertEqual(durable_at_send[0]["phase"], "write_intent")
        self.assertNotIn("assessment_diagnostic", durable_at_send[0])
        self.assertEqual(durable_at_send[0]["payload"], payload)
        self.assertEqual(self.writes[0][1]["payload"], payload)
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.reservations, [])
        self.assertEqual(self.accounts, [])
        self.assertEqual(self.state()["jobs"], [])
        self.assertEqual(self.state()["counts"]["verified"], 1)
        self.assertEqual(self.state()["receipts"][-1]["native"]["readback_status"], "verified")
        self.assertFalse(self.step(runtime, native_write=native)[0])
        self.assertEqual(len(self.writes), 1)

    def test_optional_diagnostics_drop_before_aggregate_core_state_cap(self):
        self.closed()
        self.step(FakeRuntime(self, reason="abstained"))
        self.remove_context_diagnostics()
        self.source_start("turn-2")
        self.assertEqual(jobs.notice(self.config, self.event("turn-2"))["outcome"], "admitted")
        data = self.state()
        data["jobs"][0]["assessment_diagnostic"] = {
            "runtime_reason": "unknown", "validation_reason": "unknown"}
        self.assertIn("assessment_diagnostic", data["receipts"][0])
        core = json.loads(json.dumps(data))
        for item in core["jobs"] + core["receipts"]:
            item.pop("assessment_diagnostic", None)
        cap = len(jobs.encoded(core))
        self.assertGreater(len(jobs.encoded(data)), cap)
        self.assertTrue(all(len(jobs.encoded(job)) <= jobs.MAX_JOB_BYTES for job in data["jobs"]))
        with patch.object(jobs, "MAX_STATE", cap):
            jobs._write(jobs._path(self.config, SESSION), data)
            self.assertEqual(self.state(), core)
            self.assertEqual(jobs._path(self.config, SESSION).stat().st_size, cap)
        self.assertEqual(self.state()["counts"], core["counts"])
        self.assertEqual(self.writes, [])

    def test_optional_diagnostic_removal_never_rescues_oversized_core_state(self):
        self.closed()
        original = self.state()
        path = jobs._path(self.config, SESSION)
        before = path.read_bytes()
        diagnostic = {"runtime_reason": "timeout", "validation_reason": "unknown"}
        with self.subTest(boundary="per_job"):
            data = json.loads(json.dumps(original))
            job = data["jobs"][0]
            job["padding"] = ""
            job["padding"] = "x" * (jobs.MAX_JOB_BYTES + 1 - len(jobs.encoded(job)))
            self.assertEqual(len(jobs.encoded(job)), jobs.MAX_JOB_BYTES + 1)
            job["assessment_diagnostic"] = diagnostic
            with self.assertRaisesRegex(ValueError, "recording_state_cap"):
                jobs._write(path, data)
            self.assertEqual(path.read_bytes(), before)
        with self.subTest(boundary="aggregate"):
            data = json.loads(json.dumps(original))
            core_bytes = len(jobs.encoded(data))
            data["jobs"][0]["assessment_diagnostic"] = diagnostic
            with patch.object(jobs, "MAX_STATE", core_bytes - 1):
                with self.assertRaisesRegex(ValueError, "recording_state_cap"):
                    jobs._write(path, data)
            self.assertEqual(path.read_bytes(), before)
        self.assertEqual(self.state(), original)
        self.assertEqual((self.reservations, self.accounts, self.writes), ([], [], []))

    def test_export_drops_optional_diagnostic_before_per_record_core_cap(self):
        self.closed()
        self.step(FakeRuntime(self, reason="abstained"))
        self.remove_context_diagnostics()
        expected = jobs.export_receipts(self.config, [SESSION])
        expected["receipts"][0].pop("assessment_diagnostic")
        cap = len(jobs.encoded(expected["receipts"][0]))
        with patch.object(jobs, "MAX_RECEIPT_BYTES", cap):
            exported = jobs.export_receipts(self.config, [SESSION])
        self.assertEqual(exported, expected)
        self.assertEqual(len(jobs.encoded(exported["receipts"][0])), cap)
        self.assertFalse(exported["truncated"])
        self.assertFalse(exported["unknown"])
        self.assertIn("assessment_diagnostic", self.state()["receipts"][0])

    def test_receipt_pressure_drops_observation_before_core_source_proof(self):
        self.closed()
        data = self.state()
        observation = {"source_turn": "closed_verified", "mode": "selected_suffix",
                       "observed_records": 91, "selected_records": 25, "omitted_records": 66}
        data["jobs"][0]["observation"] = observation
        baseline = copy.deepcopy(data)
        jobs._finish(baseline, baseline["jobs"][0], "deferred", "assessment_refused")
        complete = baseline["receipts"][0]
        self.assertEqual(complete["observation"], observation)
        core = {key: value for key, value in complete.items() if key != "observation"}
        with patch.object(jobs, "MAX_RECEIPT_BYTES", len(jobs.encoded(core)) + 192):
            jobs._finish(data, data["jobs"][0], "deferred", "assessment_refused")
        self.assertEqual(data["receipts"][0], core)
        self.assertIn("source", data["receipts"][0])

    def test_export_drops_optional_observation_before_per_record_core_cap(self):
        self.closed()
        self.step(FakeRuntime(self, reason="abstained"))
        self.remove_context_diagnostics()
        expected = jobs.export_receipts(self.config, [SESSION])
        expected["receipts"][0].pop("assessment_diagnostic")
        expected["receipts"][0].pop("observation")
        cap = len(jobs.encoded(expected["receipts"][0]))
        with patch.object(jobs, "MAX_RECEIPT_BYTES", cap):
            exported = jobs.export_receipts(self.config, [SESSION])
        self.assertEqual(exported, expected)
        self.assertFalse(exported["truncated"])
        self.assertIn("observation", self.state()["receipts"][0])

    def test_export_drops_optional_diagnostics_before_global_core_cap(self):
        self.closed()
        self.step(FakeRuntime(self, reason="abstained"))
        self.source_start("turn-2")
        self.assertEqual(jobs.notice(self.config, self.event("turn-2"))["outcome"], "admitted")
        self.source_complete("turn-2")
        jobs.close_turn(self.config, SESSION, "turn-2")
        self.step(FakeRuntime(self, reason="abstained"))
        self.remove_context_diagnostics()
        expected = jobs.export_receipts(self.config, [SESSION])
        self.assertEqual(len(expected["receipts"]), 2)
        for receipt in expected["receipts"]:
            receipt.pop("assessment_diagnostic")
            receipt.pop("observation")
        cap = len(jobs.encoded(expected))
        with patch.object(jobs, "MAX_RECEIPTS_BYTES", cap):
            exported = jobs.export_receipts(self.config, [SESSION])
        self.assertEqual(exported, expected)
        self.assertEqual(len(jobs.encoded(exported)), cap)
        self.assertFalse(exported["truncated"])
        self.assertFalse(exported["unknown"])
        self.assertTrue(all("assessment_diagnostic" in receipt for receipt in self.state()["receipts"]))


class MiscRecordingJobsTests(unittest.TestCase, RecordingFixture):
    def setUp(self):
        RecordingFixture.__init__(self, self)
        self.root = self.root.resolve()
        self.workspace = self.root / "unconfigured"
        self.workspace.mkdir()
        self.service.write_text(json.dumps({"mode": "connect", "url": "http://127.0.0.1:18766/",
            "database_name": "project", "database_path": "/srv/mneme/misc.db"}))
        self.binding = {"workspace_root": str(self.workspace), "workspace_origin": str(self.workspace)}
        self.hook = self.root / "device-hook.json"
        self.hook.write_text("{}")
        self.config.update(schema="mneme.codex-hooks.config.v11", memory_scope="misc",
            memory_mode="async", project_root=self.workspace, service_config=self.service.resolve(),
            workspace_binding=self.binding, excluded_roots=[], _config_path=self.hook,
            store_target={"db_alias": "project", "database_path": "/srv/mneme/misc.db", "db_id": DB_ID})

    def test_notes_and_episodes_keep_original_workspace_provenance(self):
        from urllib.parse import parse_qs, urlsplit
        for kind in ("lesson", "episode"):
            with self.subTest(kind=kind):
                if self.path().exists():
                    self.path().unlink()
                if jobs._path(self.config, SESSION).exists():
                    jobs._path(self.config, SESSION).unlink()
                job = self.closed()
                _, runtime = self.step(FakeRuntime(self, kind=kind))
                self.assertEqual(len(runtime.calls), 1)
                payload = self.writes[-1][1]["payload"]
                self.assertEqual(payload["source"]["key"], job["key"])
                self.assertEqual(parse_qs(urlsplit(payload["source"]["reference"]).query)["workspace"],
                                 [str(self.workspace)])
                self.assertTrue(payload["body"].startswith("Workspace: " + str(self.workspace) + "\n\n"))
                self.assertEqual(self.writes[-1][1]["workspace_binding"], self.binding)
                self.assertEqual(self.writes[-1][1]["db_alias"], "project")
                self.assertEqual(self.state()["counts"]["verified"], 1)
                if kind == "episode":
                    self.assertEqual(payload["action"], "append")

    def test_misc_routing_and_conditional_witnesses_decode_with_exact_source(self):
        import routing_memory
        origin = self.root / "routing-origin"
        origin.symlink_to(self.workspace, target_is_directory=True)
        self.binding = {"workspace_root": str(self.workspace), "workspace_origin": str(origin)}
        self.config["workspace_binding"] = self.binding
        for conditional in (False, True):
            with self.subTest(conditional=conditional):
                if self.path().exists():
                    self.path().unlink()
                if jobs._path(self.config, SESSION).exists():
                    jobs._path(self.config, SESSION).unlink()
                route = RecordingJobsTests.routed_closed(self, conditional=conditional)
                _, runtime = self.step(FakeRuntime(self, routing=True))
                self.assertEqual(len(runtime.calls), 1)
                payload = self.writes[-1][1]["payload"]
                self.assertEqual(payload["source"]["reference"], f"codex://{SESSION}/turn-1")
                self.assertEqual(payload["source"]["namespace"], routing_memory.NAMESPACE)
                body = payload["body"]
                history = {"id": OTHER_DB, "db_id": DB_ID, "status": "active",
                    "memory_kind": {"kind": "semantic"}, "tags": payload["tags"],
                    "body": body, "provenance": {"type": "external", "source": payload["source"]},
                    "body_range": {"source_start": 0, "source_end": len(body.encode()),
                                   "has_more": False, "next_offset": None}}
                decoded = routing_memory.decode_witness(history, expected_db_id=DB_ID)
                self.assertIsNotNone(decoded)
                witness = decoded["witness"]
                self.assertEqual(witness["binding"], route)
                self.assertEqual(witness.get("entry_kind"), "conditional" if conditional else None)
                self.assertTrue(witness["note"].startswith("Workspace: " + str(origin) + "\n\n"))
                self.assertEqual(self.state()["receipts"][-1]["proposal"]["body"], witness["note"])
                self.assertEqual(self.state()["receipts"][-1]["routing"]["outcome"], "included")
                self.assertFalse(self.step(runtime)[0])

    def test_routing_workspace_provenance_cap_omits_annotation_not_authored_note(self):
        from dataclasses import replace
        import routing_memory
        RecordingJobsTests.routed_closed(self)
        class FullNote(FakeRuntime):
            def assess(self, *args, **kwargs):
                result = super().assess(*args, **kwargs)
                result["proposal"] = replace(result["proposal"], body="N" * routing_memory.MAX_NOTE_BYTES)
                return result
        _, runtime = self.step(FullNote(self, routing=True))
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(len(self.writes), 1)
        payload = self.writes[0][1]["payload"]
        self.assertEqual(payload["source"]["namespace"], "codex-acquisition.v1")
        self.assertNotIn("tags", payload)
        self.assertIn("?scope=misc&workspace=", payload["source"]["reference"])
        self.assertEqual(payload["body"], "Workspace: " + str(self.workspace) + "\n\n" + "N" * routing_memory.MAX_NOTE_BYTES)
        receipt = self.state()["receipts"][-1]
        self.assertEqual(receipt["routing"], {"outcome": "omitted", "reason": "routing_workspace_provenance_cap"})
        self.assertEqual(receipt["proposal"]["body"], payload["body"])
        self.assertEqual(receipt["outcome"], "verified")

    def test_long_origin_omits_optional_witness_and_keeps_full_ordinary_provenance(self):
        import routing_memory
        from test_routing_memory import binding
        route = binding()
        route["db_id"] = DB_ID
        body = routing_memory.encode_witness(note="Original authored note", binding=route, sign="boost",
            conditions="Relevant condition", rationale="Observed correction", shown_summary="Shown advice",
            session=SESSION, turn="turn-1", evidence=[
                {"kind": "memory_delivery", "reference": "delivered-card"},
                {"kind": "user_statement", "reference": "user-choice"}])
        original = {"namespace": routing_memory.NAMESPACE, "tags": [routing_memory.TAG], "body": body}
        origin = "/" + "記憶" * 600
        omitted, reason = jobs._misc_routing_provenance(original, origin, db_id=DB_ID)
        self.assertIsNone(omitted)
        self.assertEqual(reason, "routing_workspace_provenance_cap")
        self.assertEqual(original["body"], body)
        payload = {"source": {"namespace": "codex-acquisition.v1", "key": "unchanged",
                   "reference": f"codex://{SESSION}/turn-1"}, "body": "Original authored note"}
        jobs._misc_provenance(payload, origin)
        self.assertEqual(payload["body"], "Workspace: " + origin + "\n\nOriginal authored note")
        self.assertEqual(payload["source"]["namespace"], "codex-acquisition.v1")

    def test_lexical_workspace_origin_not_canonical_alias_is_remembered(self):
        from urllib.parse import parse_qs, urlsplit
        alias = self.root / "workspace-alias"
        alias.symlink_to(self.workspace, target_is_directory=True)
        self.binding = {"workspace_root": str(self.workspace), "workspace_origin": str(alias)}
        self.config["workspace_binding"] = self.binding
        self.closed()
        _, runtime = self.step()
        self.assertEqual(len(runtime.calls), 1)
        payload = self.writes[0][1]["payload"]
        self.assertEqual(parse_qs(urlsplit(payload["source"]["reference"]).query)["workspace"], [str(alias)])
        self.assertTrue(payload["body"].startswith("Workspace: " + str(alias) + "\n\n"))

    def test_recording_off_neither_admits_nor_calls_provider_or_writer(self):
        self.config["recording_mode"] = "off"
        self.startup()
        self.source_start()
        self.assertEqual(jobs.notice(self.config, self.event()), {"outcome": "skipped"})
        worked, runtime = self.step()
        self.assertFalse(worked)
        self.assertEqual(runtime.calls, [])
        self.assertEqual(self.writes, [])
        self.assertFalse(jobs._path(self.config, SESSION).exists())

    def test_long_unicode_workspace_keeps_full_body_with_bounded_reference(self):
        origin = "/" + "記憶" * 600
        payload = {"source": {"key": "unchanged", "reference": "codex://session/turn"}, "body": "Original body"}
        jobs._misc_provenance(payload, origin)
        self.assertLessEqual(len(payload["source"]["reference"].encode()), 2048)
        self.assertIn("workspace_sha256=" + jobs.digest(origin.encode()), payload["source"]["reference"])
        self.assertEqual(payload["source"]["key"], "unchanged")
        self.assertEqual(payload["body"], "Workspace: " + origin + "\n\nOriginal body")

    def test_same_session_cannot_rebind_or_reset_recording_accounting(self):
        self.begin()
        before = jobs._path(self.config, SESSION).read_bytes()
        other = self.root / "other"
        other.mkdir()
        current = {**self.config, "project_root": other,
            "workspace_binding": {"workspace_root": str(other), "workspace_origin": str(other)}}
        jobs.session_start(current, {"session_id": SESSION, "source": "startup"})
        self.assertFalse(jobs._transaction(current, SESSION, lambda d: (d.update(counts={}), True))[0])
        self.assertEqual(jobs._path(self.config, SESSION).read_bytes(), before)

    def test_job_origin_tampering_refuses_all_native_and_provider_work(self):
        self.closed()
        self.mutate(lambda d: d["jobs"][0]["workspace_binding"].update(workspace_origin="/tmp"))
        _, runtime = self.step()
        self.assertEqual(runtime.calls, [])
        self.identity.assert_not_called()
        self.overlap.assert_not_called()
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["counts"]["deferred"], 1)

    def test_exclusions_added_after_admission_block_native_and_provider(self):
        self.closed()
        before = self.state()["counts"].copy()
        self.hook.write_text(json.dumps({"excluded_roots": [str(self.workspace)]}))
        _, runtime = self.step()
        self.assertEqual(runtime.calls, [])
        self.identity.assert_not_called()
        self.overlap.assert_not_called()
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["counts"]["admitted"], before["admitted"])
        self.assertEqual(self.state()["counts"]["deferred"], 1)

    def test_config_drift_after_reserve_settles_no_call_and_never_writes(self):
        self.closed()
        def reserve(key):
            self.reservations.append(key)
            self.hook.write_text('{"changed":true}')
            return True
        _, runtime = self.step(reserve=reserve)
        self.assertEqual(runtime.calls, [])
        self.assertEqual(len(self.accounts), 1)
        self.assertFalse(self.accounts[0][1]["provider_attempt"])
        self.assertEqual(self.writes, [])
        self.assertEqual(self.state()["counts"]["admitted"], 1)

    def test_reconfigured_workspace_after_assessment_cannot_write(self):
        self.closed()
        owner = self
        class ChangedRuntime(FakeRuntime):
            def assess(self, *args, **kwargs):
                result = super().assess(*args, **kwargs)
                (owner.workspace / ".mneme").mkdir()
                return result
        _, runtime = self.step(ChangedRuntime(self))
        self.assertEqual(len(runtime.calls), 1)
        self.assertEqual(len(self.accounts), 1)
        self.assertEqual(self.writes, [])
        # The existing ledger and paid/source identities remain; no fresh allowance.
        data = jobs._load(jobs._path(self.config, SESSION), SESSION)
        self.assertEqual(data["counts"]["admitted"], 1)
        self.assertEqual(data["workspace_binding"], self.binding)

    def test_immediate_native_save_recheck_after_catalog_prevents_write(self):
        from target_policy import misc_policy
        policy = misc_policy(self.config)
        job = {"config_sha256": jobs._config_digest(self.config), "db_id": DB_ID,
            "db_alias": "project", "store_target": policy.canonical(), "workspace_binding": self.binding,
            "proposal_kind": "lesson", "payload": {"summary": "A fact", "body": "A body"}}
        catalog = [{"db": "project", "name": "project", "state": "open",
                    "configured_path": "/srv/mneme/misc.db", "db_id": DB_ID}]
        with patch("mcp_client.McpClient") as factory:
            client = factory.return_value
            def catalog_then_enroll(*args):
                (self.workspace / ".mneme").mkdir()
                return catalog
            client.call_tool.side_effect = catalog_then_enroll
            with self.assertRaises(ValueError):
                jobs._native_write(self.config, job, 2)
            client.save_verified.assert_not_called()
            client.close.assert_called_once()


if __name__ == "__main__":
    unittest.main()

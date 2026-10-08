"""Resource-sized recording nomination; synthetic owners, no provider/native I/O."""
import copy
import json
import unittest
from unittest.mock import patch

import hook_recall as recall
import recording_contract as contract
from librarian_policy import LibrarianBudget, resolve
from reader_runtime import ReaderRuntime
import fixture_recall_owner as hook_fixtures
import test_recording_contract as contract_fixtures
import fixture_recording as job_fixtures
import routing_memory as routing
import test_routing_memory as routing_fixtures

observation = contract_fixtures.observation
statement = contract_fixtures.statement
memory_marker = contract_fixtures.memory_marker


def nominees(count, summary="short prior note"):
    return [{"id": f"{index + 1:026d}", "kind": "semantic", "summary": summary}
            for index in range(count)]


def payload(prompt):
    return json.loads(prompt[len(contract.PROMPT_PREFIX):])


class RecordingOverlapTests(unittest.TestCase):
    def owner(self, count=12, padding=0):
        owner = hook_fixtures.OverlapFixture(self)
        cards = nominees(count)
        owner.context.update(primary=[{"id": c["id"]} for c in cards], expansions=[], episodes=[],
                             **hook_fixtures.discovery_metadata())
        owner.nodes = {c["id"]: {**owner._node(c["id"], "active", c["summary"]),
                                 "envelope_padding": "x" * padding} for c in cards}
        return owner, hook_fixtures.FakeClient(owner.catalog, owner.context, owner.nodes)

    def routing_owner(self, count=2):
        owner, client = self.owner(count)
        for node in owner.nodes.values():
            node['tags'] = [routing.TAG]
            node['provenance']['source'].update(namespace=routing.NAMESPACE, session='session1',
                                               reference='codex://session1/turn1')
        return owner, client

    def test_escaped_optional_body_overrun_keeps_guarded_summaries_and_stops_enrichment(self):
        owner, client = self.routing_owner()
        binding = routing_fixtures.binding()
        binding['db_id'] = owner.db_id
        body = routing_fixtures.body(binding=binding, note='\\' * 1908,
                                     conditions='\\' * 500, rationale='\\' * 600,
                                     shown_summary='\\' * 700)
        identifier = next(iter(owner.nodes))
        sidecar = {**owner.nodes[identifier], 'db_id':owner.db_id,
                   'memory_kind':{'kind':'semantic'}, 'body':body,
                   'body_range':{'source_start':0,'source_end':len(body.encode()),
                                 'has_more':False,'next_offset':None}}
        self.assertIsNotNone(routing.decode_witness(sidecar, expected_db_id=owner.db_id))
        body_bytes, response_bytes = len(body.encode()), recall._bytes(sidecar)
        self.assertLessEqual(body_bytes,routing.MAX_BODY_BYTES)
        self.assertGreater(response_bytes,routing.MAX_BODY_BYTES + 4096)
        required = 2 * recall._bytes(owner.catalog) + recall._bytes(owner.context)
        required += sum(recall._bytes(node) for node in owner.nodes.values())
        # The candidate's old raw-body + guessed-envelope reserve admits this
        # legal response, but its actual escaped wire bytes exceed that reserve.
        allowance = required + routing.MAX_BODY_BYTES + 4096
        class Budget(LibrarianBudget):
            native_read_bytes = property(lambda self: allowance)
        original = client.call_tool
        def read(name, args):
            if name == 'get' and args['body']:
                client.calls.append((name,args))
                return sidecar
            return original(name,args)
        client.call_tool = read
        result = owner.overlap(client, budget=Budget(), include_routing=True)
        self.assertEqual(result['outcome'],'ok')
        self.assertEqual(len(result['cards']),2)
        self.assertTrue(all('routing_witness' not in card for card in result['cards']))
        self.assertEqual(result['native_work']['decoded_bytes'],required + response_bytes)
        self.assertGreater(result['native_work']['decoded_bytes'],allowance)
        self.assertEqual([name for name,_ in client.calls],
                         ['databases','recall_context','get','get','databases','get'])
        self.assertEqual(client.calls[-1][1]['expected_db_id'],owner.db_id)

    def test_optional_failure_preserves_summaries_but_known_owner_or_config_drift_does_not(self):
        for failure in ('timeout','decode','owner','config'):
            with self.subTest(failure=failure):
                owner, client = self.routing_owner()
                original = client.call_tool
                def read(name,args):
                    if name == 'get' and args['body']:
                        client.calls.append((name,args))
                        if failure == 'timeout':
                            raise TimeoutError('optional history deadline')
                        if failure == 'owner':
                            raise recall.McpError('expected_db_id mismatch for database project')
                        if failure == 'config':
                            owner.config.write_bytes(owner.config.read_bytes() + b'\n')
                        return {**owner.nodes[args['id']], 'body':'not a routing witness'}
                    return original(name,args)
                client.call_tool = read
                result = owner.overlap(client, include_routing=True)
                expected = 'identity_mismatch' if failure in ('owner','config') else 'ok'
                self.assertEqual(result['outcome'],expected)
                self.assertEqual(len(result['cards']),0 if expected == 'identity_mismatch' else 2)
                self.assertTrue(all('routing_witness' not in card for card in result['cards']))
                optional_index = next(index for index,(name,args) in enumerate(client.calls)
                                      if name == 'get' and args['body'])
                self.assertEqual(client.calls[optional_index-1],('databases',{}))

    def test_initial_identity_bytes_are_in_the_shared_overlap_phase_total(self):
        owner, client = self.owner(2)
        with patch('hook_recall.McpClient',return_value=client):
            identity = recall.resolve_project_identity(owner.config,owner.root)
        spent = identity['native_work']['decoded_bytes']
        self.assertEqual(spent,recall._bytes(owner.catalog))
        client.calls.clear()
        result = owner.overlap(client,spent_native_bytes=spent)
        expected = spent + 2 * recall._bytes(owner.catalog) + recall._bytes(owner.context)
        expected += sum(recall._bytes(node) for node in owner.nodes.values())
        self.assertEqual(result['native_work']['decoded_bytes'],expected)
        for invalid in (True,-1,1.5,LibrarianBudget().native_read_bytes+1):
            client.calls.clear()
            rejected = owner.overlap(client,spent_native_bytes=invalid)
            self.assertEqual(rejected['outcome'],'unavailable')
            self.assertEqual(client.calls,[])

    def test_legal_window_beyond_four_reaches_one_abstaining_assessor(self):
        owner, client = self.owner()
        result = owner.overlap(client, budget=LibrarianBudget(effort="low"))
        self.assertEqual(result["outcome"], "ok")
        self.assertEqual(len(result["cards"]), 12)
        runtime = object.__new__(ReaderRuntime)
        runtime.budget = LibrarianBudget(effort="low")
        calls = []
        def assess(preparation, **options):
            prompt, context = preparation()
            calls.append((prompt, context))
            value = contract.validate_answer({"proposal": None}, context)
            return {"assessment": value, "reason": options["success"](value),
                    "provider_attempt": False, "usage": None}
        with patch.object(runtime, "_fresh_assessment", side_effect=assess):
            assessed = runtime.assess(observation(), result["cards"])
        self.assertEqual(assessed["reason"], "abstained")
        self.assertEqual(len(calls), 1)
        self.assertEqual(len(payload(calls[0][0])["overlap_cards"]), 12)
        self.assertIsNone(assessed["proposal"])
        self.assertEqual(assessed["maintenance"], [])
        self.assertTrue(all(not a["body"] and not a["edges"] and a["expected_db_id"] == owner.db_id
                            for name, a in client.calls if name == "get"))

    def test_native_bytes_return_guarded_prefix_and_honest_unread_count(self):
        for effort in ("low", "medium"):
            owner, client = self.owner(60, padding=600)
            budget = LibrarianBudget(effort=effort)
            result = owner.overlap(client, budget=budget)
            self.assertEqual(result["outcome"], "ok")
            counts = result["discovery"]["adapter"]
            self.assertEqual(counts["observed_unique_window"], 60)
            self.assertEqual(counts["returned"] + counts["work_budget_unread"], 60)
            self.assertEqual(counts["returned"], len(result["cards"]))
            self.assertLessEqual(result["native_work"]["decoded_bytes"], budget.native_read_bytes)
            self.assertEqual(result["native_work"]["decoded_bytes"],
                             2 * recall._bytes(owner.catalog) + recall._bytes(owner.context) +
                             sum(recall._bytes(owner.nodes[a["id"]]) for n, a in client.calls if n == "get"))
            self.assertEqual(client.calls[-1], ("databases", {}))
            if effort == "low":
                self.assertGreater(counts["work_budget_unread"], 0)
                low_count = len(result["cards"])
            else:
                self.assertGreater(len(result["cards"]), low_count)

    def test_remaining_time_stops_hydration_but_preserves_final_guard(self):
        owner, client = self.owner()
        current = [0.0]
        original = client.call_tool
        def read(name, args):
            value = original(name, args)
            if name == "get":
                current[0] = .91
            return value
        client.call_tool = read
        with patch("hook_recall.time.monotonic", side_effect=lambda: current[0]):
            result = owner.overlap(client, timeout=1)
        self.assertEqual(result["outcome"], "ok")
        self.assertEqual(len(result["cards"]), 1)
        self.assertEqual(result["discovery"]["adapter"]["work_budget_unread"], 11)
        self.assertEqual(client.calls[-1], ("databases", {}))

    def test_late_stale_guard_or_final_identity_failure_discards_prefix(self):
        for failure in ("stale", "guard", "final"):
            owner, client = self.owner()
            original = client.call_tool
            calls = [0]
            def read(name, args):
                value = original(name, args)
                if name == "get":
                    calls[0] += 1
                    if calls[0] == 6:
                        if failure == "stale":
                            return {**value, "status": "archived"}
                        if failure == "guard":
                            raise recall.McpError("expected_db_id mismatch for database project")
                if name == "databases" and calls[0] and failure == "final":
                    return [{**value[0], "db_id": "2" * 26}]
                return value
            client.call_tool = read
            result = owner.overlap(client)
            self.assertIn(result["outcome"], ("identity_mismatch", "unavailable"))
            self.assertEqual(result["cards"], [])
            self.assertEqual(result["discovery"]["adapter"]["returned"], 0)
            self.assertTrue(all(a.get("expected_db_id") == owner.db_id for n, a in client.calls if n == "get"))

    def test_exact_authored_room_admits_short_hints_after_large_misses(self):
        source = observation([statement('Observed constraint: "é".')])
        _, base = contract.prepare(source)
        cards = nominees(12, '\n"' * 125)
        cards[-6:] = nominees(6, "short")
        for index, card in enumerate(cards):
            card["id"] = f"{index + 1:026d}"
        before = copy.deepcopy((source, cards))
        with patch.object(contract, "MAX_AUTHORED_BYTES", base.authored_bytes + 500):
            prompt, context = contract.prepare(source, cards)
            plan, reason = contract.overlap_plan(source)
            self.assertIsNone(reason)
            self.assertGreater(plan["max_nodes"], 4)
            self.assertLessEqual(context.authored_bytes, contract.MAX_AUTHORED_BYTES)
        self.assertEqual(context.bindings, base.bindings)
        self.assertGreater(len(payload(prompt)["overlap_cards"]), 4)
        self.assertEqual(context.authored_bytes, len(contract.instructions_for(context).encode()) +
                         len(contract._encoded(contract.schema_for(context))) + len(prompt.encode()))
        self.assertEqual(context.dropped_overlap_cards + len(payload(prompt)["overlap_cards"]), 12)
        self.assertEqual((source, cards), before)

    def test_effort_prompt_bytes_constrain_hints_without_cutting_evidence(self):
        source = observation([statement("Mandatory observed decision.")])
        _, base = contract.prepare(source)
        counts = []
        for effort in ("low", "medium", "high"):
            budget = LibrarianBudget(effort=effort)
            prompt, context = contract.prepare(source, nominees(90, "é" * 256), budget=budget)
            self.assertEqual(context.bindings, base.bindings)
            self.assertLessEqual(context.authored_bytes - base.authored_bytes, budget.recording_hint_bytes)
            counts.append(len(payload(prompt)["overlap_cards"]))
        self.assertLess(counts[0], counts[1])
        self.assertLess(counts[1], counts[2])

    def test_evidence_at_ceiling_beats_even_growing_omission_metadata(self):
        fixture = contract_fixtures.RecordingPrepareTests()
        source = fixture.at_authored_budget()
        _, base = contract.prepare(source)
        prompt, context = contract.prepare(source, nominees(100))
        self.assertEqual(context.bindings, base.bindings)
        self.assertEqual(context.dropped_overlap_cards, 100)
        self.assertEqual(payload(prompt)["overlap_cards"], [])
        self.assertLessEqual(context.authored_bytes, contract.MAX_AUTHORED_BYTES)
        plan, _ = contract.overlap_plan(source)
        self.assertEqual(plan["max_nodes"], 0)

    def test_historical_delivered_binding_cannot_become_a_fresh_alias(self):
        source = observation([statement("Scoped correction."), memory_marker()])
        cards = nominees(12)
        cards[5]["id"] = "1" * 26
        prompt, context = contract.prepare(source, cards)
        target = next(t for t in context.association_bindings if t.native_id == "1" * 26)
        self.assertEqual((target.opaque_id, target.origin, target.full_get_fingerprint),
                         ("shown001", "delivery", "a" * 64))
        self.assertEqual(context.dropped_overlap_cards, 1)
        self.assertEqual(len(payload(prompt)["overlap_cards"]), 11)

    def test_zero_authored_room_skips_discovery_with_identity_still_checked(self):
        owner, client = self.owner()
        result = owner.overlap(client, read_plan=LibrarianBudget().overlap_window(0))
        self.assertEqual(result["outcome"], "empty")
        self.assertEqual(client.calls, [("databases", {}), ("databases", {})])
        self.assertEqual(result["discovery"]["native"]["state"], "unknown")

    def test_oversized_native_response_refuses_further_hydration(self):
        owner, client = self.owner()
        owner.nodes[nominees(1)[0]["id"]]["envelope_padding"] = "x" * LibrarianBudget().native_read_bytes
        result = owner.overlap(client)
        self.assertEqual(result["outcome"], "unavailable")
        self.assertEqual(result["cards"], [])
        self.assertEqual([n for n, _ in client.calls], ["databases", "recall_context", "get"])
        self.assertGreater(result["native_work"]["decoded_bytes"], result["native_work"]["read_allowance_bytes"])

    def test_ordinary_hook_defaults_are_unchanged(self):
        owner, client = self.owner()
        with patch("hook_recall.McpClient", return_value=client):
            result = recall._collect(owner.config, "task cue", owner.root, 1.5)
        self.assertEqual(len(result["cards"]), 2)
        self.assertEqual(client.calls[1], ("recall_context", {"db": "project", "text": "task cue",
                                                            "k": 2, "max_nodes": 4, "depth": 0}))
        self.assertNotIn("native_work", result)

    def test_job_and_runtime_use_identical_shared_preparation_for_each_effort(self):
        for effort in ("low", "medium", "high"):
            owner = job_fixtures.RecordingFixture(self)
            owner.config["librarian_effort"] = effort
            owner.closed()
            cards = nominees(90, "é" * 256)
            owner.overlap.return_value = {"outcome": "ok", "cards": cards, "db_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV"}
            runtime = job_fixtures.FakeRuntime(owner, reason="abstained")
            actual = object.__new__(ReaderRuntime)
            actual.budget = resolve(owner.config)
            job_preparations, runtime_preparations = [], []
            original_prepare = contract.prepare
            def prepare(*args, **kwargs):
                value = original_prepare(*args, **kwargs)
                job_preparations.append(value)
                return value
            def fresh(preparation, **options):
                runtime_preparations.append(preparation())
                return {"assessment": {"proposal": None, "maintenance": []}, "reason": "abstained",
                        "provider_attempt": True, "usage": {"input_tokens": 20, "output_tokens": 5}}
            def assess(observed, overlap, *, timeout):
                with patch.object(actual, "_fresh_assessment", side_effect=fresh):
                    return actual.assess(observed, overlap, timeout=timeout)
            runtime.assess = assess
            with patch.object(contract, "prepare", side_effect=prepare):
                owner.step(runtime)
            self.assertEqual(runtime_preparations, [job_preparations[-2]])
            self.assertEqual(owner.overlap.call_args.kwargs["budget"], actual.budget)
            self.assertEqual((len(owner.reservations), len(owner.accounts), len(owner.writes)), (1, 1, 0))
            owner.close()


if __name__ == "__main__":
    unittest.main()

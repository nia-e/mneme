"""Authored contract counterexamples; fake answers do not establish model quality."""
import copy
import hashlib
import json
import unittest

import routing_contract as contract
from test_routing_memory import binding, body, DB

CURRENT = [{"source": "turn:1:user", "text": "The snapshot includes every committed record."},
           {"source": "turn:2:tool", "text": "No newer committed journal entries remain."}]


def witness(n=4, sign="boost", **changes):
    value = json.loads(body(sign=sign, **changes))
    return {"node_id": f"{n:026d}", "body_sha256": hashlib.sha256(str(n).encode()).hexdigest(), "witness": value}


def prepared(history):
    prompt, context = contract.prepare(CURRENT, history, expected_db_id=DB)
    assert prompt is not None, context
    return prompt, context


def answer(context, verdicts=None, corrections=()):
    history = json.loads(context.snapshot_json)["history"]
    verdicts = verdicts or ["matched"] * len(history)
    return {"judgments": [{"witness": x["id"], "applicability": verdict,
        "current_refs": json.loads(context.snapshot_json)["sources"], "correction_applicable": x["id"] in corrections}
        for x, verdict in zip(history, verdicts)]}


def result(history, verdicts=None, corrections=()):
    _, context = prepared(history)
    return contract.validate_answer(answer(context, verdicts, corrections), context)


class RoutingContractTests(unittest.TestCase):
    def test_conditional_origin_projects_without_separating_opposed_route_group(self):
        positive = witness(entry_kind="conditional")
        negative = witness(5, "weaken")
        prompt, context = prepared([positive, negative])
        history = json.loads(prompt)["history"]
        self.assertEqual(history[0]["entry_kind"], "conditional")
        self.assertNotIn("entry_kind", history[1])
        self.assertEqual(history[0]["route"], history[1]["route"])
        self.assertEqual(contract.validate_answer(answer(context), context).hints, ())
        correction = witness(6, "weaken", entry_kind="conditional",
                             corrects={"node_id": positive["node_id"], "body_sha256": positive["body_sha256"]})
        self.assertEqual(result([positive, correction], corrections=("w2",)).hints[0]["sign"], "weaken")

    def test_fixed_wire_no_native_identifiers_in_prompt(self):
        prompt, context = prepared([witness()])
        for hidden in (DB, binding()["route"]["target"], "a" * 64, "source-record:5", "session1"):
            self.assertNotIn(hidden, prompt)
        out = contract.validate_answer(answer(context), context)
        self.assertEqual(out.hints, ({**binding(), "sign": "boost"},))
        self.assertEqual(context.authored_bytes, len(contract.INSTRUCTIONS.encode()) +
                         len(contract.encoded(contract.schema(context))) + len(prompt.encode()))

    def test_opposite_unknown_never_invert(self):
        for verdict in ("opposite", "unknown"):
            self.assertEqual(result([witness(sign="weaken")], [verdict]).hints, ())

    def test_matched_opposing_opinions_neutral(self):
        self.assertEqual(result([witness(), witness(5, "weaken")]).hints, ())

    def test_unknown_opposition_blocks_but_unknown_same_sign_does_not(self):
        self.assertEqual(result([witness(), witness(5, "weaken")], ["matched", "unknown"]).hints, ())
        self.assertEqual(len(result([witness(), witness(5)], ["matched", "unknown"]).hints), 1)
        self.assertEqual(len(result([witness(), witness(5, "weaken")], ["matched", "opposite"]).hints), 1)

    def test_scoped_negative_exclusion_vs_possible_and_matched_opposition(self):
        # Authored wire judgments, not a test that the model infers these labels.
        current = [{"source": "current:policy", "text": "Partition policy is UTC canonical, not customer-local billing; offset detail is omitted."}]
        positive = witness(conditions="Partition policy is UTC canonical.")
        negative = witness(5, "weaken", conditions="Partition policy is customer-local billing AND offset is unknown.")
        _, context = contract.prepare(current, [positive, negative], expected_db_id=DB)
        for verdict, sign, reason in (("opposite", "boost", "matched_judgment"),
                                     ("unknown", "neutral", "unknown_opposition"),
                                     ("matched", "neutral", "conflicting_opinions")):
            with self.subTest(negative_applicability=verdict):
                response = answer(context, ["matched", verdict])
                for row in response["judgments"]:
                    row["current_refs"] = ["c1"]
                out = contract.validate_answer(response, context)
                self.assertEqual(out.hints, ({**binding(), "sign": "boost"},) if sign == "boost" else ())
                self.assertEqual(out.decisions[0]["reason"], reason)

    def test_scoped_correction_and_rollback(self):
        old = witness()
        new = witness(5, "weaken", corrects={"node_id": old["node_id"], "body_sha256": old["body_sha256"]})
        self.assertEqual(result([old, new], corrections=("w2",)).hints[0]["sign"], "weaken")
        self.assertEqual(result([old, new], ["unknown", "matched"], ("w2",)).hints[0]["sign"], "weaken")
        self.assertEqual(result([old, new], ["matched", "opposite"]).hints[0]["sign"], "boost")
        self.assertEqual(result([old, new]).hints, ())  # correction not automatically applicable

    def test_missing_or_edited_old_body_cannot_resolve_conflict(self):
        old = witness()
        new = witness(5, "weaken", corrects={"node_id": old["node_id"], "body_sha256": "f" * 64})
        out = result([old, new])
        self.assertEqual(out.hints, ())
        self.assertEqual(out.decisions[0]["missing_history"], ["w2"])
        _, context = prepared([old, new])
        with self.assertRaises(ValueError):
            contract.validate_answer(answer(context, corrections=("w2",)), context)
        self.assertEqual(result([new]).hints[0]["sign"], "weaken")  # standalone opinion, not override

    def test_wrong_route_correction_not_eligible(self):
        old = witness()
        other = binding(); other["route"]["edge_fingerprint"] = "d" * 64
        new = witness(5, "weaken", binding=other,
                      corrects={"node_id": old["node_id"], "body_sha256": old["body_sha256"]})
        _, context = prepared([old, new])
        with self.assertRaises(ValueError):
            contract.validate_answer(answer(context, corrections=("w2",)), context)

    def test_cycles_and_forks_neutral(self):
        a, b = witness(), witness(5, "weaken")
        a["witness"]["corrects"] = {"node_id": b["node_id"], "body_sha256": b["body_sha256"]}
        b["witness"]["corrects"] = {"node_id": a["node_id"], "body_sha256": a["body_sha256"]}
        self.assertEqual(result([a, b], corrections=("w1", "w2")).hints, ())
        a["witness"]["corrects"] = None
        c = witness(6, "boost", corrects=b["witness"]["corrects"])
        self.assertEqual(result([a, b, c], corrections=("w2", "w3")).hints, ())

    def test_repeated_coherent_corrections_are_idempotent(self):
        old = witness()
        correction = {"node_id": old["node_id"], "body_sha256": old["body_sha256"]}
        b, c = witness(5, "weaken", corrects=correction), witness(6, "weaken", corrects=correction)
        out = result([old, b, c], corrections=("w2", "w3"))
        self.assertEqual(out.hints, ({**binding(), "sign": "weaken"},))
        prompt, _ = prepared([old, b, c])
        rows = json.loads(prompt)["history"]
        self.assertEqual(rows[1]["support"], rows[2]["support"])
        self.assertEqual(result([old, b, copy.deepcopy(b)], corrections=("w2",)).hints, out.hints)

    def test_duplicates_no_vote_or_magnitude(self):
        one = witness()
        self.assertEqual(len(result([one, copy.deepcopy(one)]).hints), 1)
        clone = witness(5)
        prompt, _ = prepared([one, clone])
        history = json.loads(prompt)["history"]
        self.assertEqual(history[0]["support"], history[1]["support"])
        self.assertEqual(len(result([one, clone]).hints), 1)
        self.assertEqual(result([one, witness(5, "weaken")]).hints, ())

    def test_chronology_and_immutable_host_snapshot(self):
        history = [witness()]
        prompt, context = prepared(history)
        self.assertEqual([x["text"] for x in json.loads(prompt)["current"]], [x["text"] for x in CURRENT])
        history[0]["witness"]["binding"]["route"]["target_fingerprint"] = "e" * 64
        self.assertEqual(contract.validate_answer(answer(context), context).hints[0]["route"], binding()["route"])

    def test_invalid_output_cannot_supply_native_ids_weights_or_missing_rows(self):
        _, context = prepared([witness()])
        for change in (lambda a: a["judgments"].clear(),
                       lambda a: a["judgments"][0].update(weight=.9),
                       lambda a: a["judgments"][0].update(witness=binding()["route"]["target"]),
                       lambda a: a["judgments"][0].update(current_refs=["historical"]),
                       lambda a: a["judgments"][0].update(current_refs=[])):
            a = answer(context); change(a)
            with self.assertRaises(ValueError): contract.validate_answer(a, context)

    def test_route_group_packing_preserves_current_and_conflicts(self):
        history = []
        for n in range(4, 12):
            route = binding(); route["route"]["edge_fingerprint"] = hashlib.sha256(str(n).encode()).hexdigest()
            history.append(witness(n, binding=route, note="x" * 2048))
        prompt, context = prepared(history)
        packet = json.loads(prompt)
        self.assertGreater(packet["omitted_history"]["witnesses"], 0)
        self.assertEqual(packet["current"], [{"id": f"c{i+1}", "text": x["text"]}
                                           for i, x in enumerate(CURRENT)])
        self.assertLessEqual(context.authored_bytes, contract.MAX_AUTHORED_BYTES)
        # An oversized eight-opinion same-route group is omitted whole, never
        # reduced to an apparently unopposed survivor.
        same = [witness(n, sign="weaken" if n % 2 else "boost", note="x" * 2048)
                for n in range(4, 12)]
        self.assertEqual(contract.prepare(CURRENT, same, expected_db_id=DB),
                         (None, "history_groups_overflow"))

    def test_same_session_different_evidence_is_not_duplicate_support(self):
        a, b = witness(), witness(5)
        b["witness"]["evidence"][1]["reference"] = "different-source-record:7"
        prompt, _ = prepared([a, b])
        rows = json.loads(prompt)["history"]
        self.assertNotEqual(rows[0]["support"], rows[1]["support"])

    def test_admission_caps_and_ordinary_prose_not_witness(self):
        self.assertEqual(contract.prepare(CURRENT, [], expected_db_id=DB), (None, "no_history"))
        for current, history in ((CURRENT * 5, [witness()]), 
                                 ([{"source": "x", "text": "x" * 5121}], [witness()]),
                                 (CURRENT, [{"summary": "similar advice"}]),
                                 (CURRENT, [witness(n, note="x" * 2048) for n in range(4, 12)])):
            self.assertIsNone(contract.prepare(current, history, expected_db_id=DB)[0])


if __name__ == "__main__":
    unittest.main()

class RoutingWindowTests(unittest.TestCase):
    @staticmethod
    def tiny_history(count, *, distinct=False):
        rows = []
        for index in range(count):
            route = binding()
            if distinct:
                route["route"]["edge_fingerprint"] = hashlib.sha256(f"route:{index}".encode()).hexdigest()
            rows.append(witness(index+4, binding=route, note="x", conditions="x",
                                rationale="x", shown_summary="x"))
        return rows

    def test_many_witnesses_few_routes_is_not_a_hint_quota(self):
        from librarian_policy import LibrarianBudget
        current = [{"source":"task", "text":"Inspect scope"}]
        plan, reason = contract.discovery_plan(current,budget=LibrarianBudget())
        self.assertIsNone(reason)
        self.assertGreater(plan["max_nodes"],34)
        self.assertEqual(plan["k"],min(64,plan["max_nodes"]))
        prompt,context = contract.prepare(current,self.tiny_history(35),expected_db_id=DB)
        self.assertIsNotNone(prompt,context)
        self.assertEqual(len(json.loads(context.snapshot_json)["history"]),35)
        self.assertLessEqual(context.authored_bytes,12288)
        self.assertLessEqual(context.worst_answer_bytes,4096)
        self.assertEqual(len(contract.validate_answer(answer(context),context).hints),1)
        self.assertEqual(contract.schema(context)["properties"]["judgments"]["maxItems"],35)
        self.assertNotIn("reason",contract.schema(context)["properties"]["judgments"]["items"]["properties"])

    def test_exact_mandatory_answer_cuts_whole_group_before_paid_call(self):
        from librarian_policy import LibrarianBudget
        current = [{"source":f"source{i}","text":"scope"} for i in range(8)]
        rows = self.tiny_history(31)
        self.assertGreater(contract.discovery_plan(current)[0]["max_nodes"],30)
        # This same tiny group fits prompt bytes but its complete answer does not.
        from unittest.mock import patch
        with patch.object(contract,"_answer_bytes",return_value=0):
            prompt,unchecked = contract.prepare(current,rows,expected_db_id=DB)
        self.assertIsNotNone(prompt,unchecked)
        snap = json.loads(unchecked.snapshot_json)
        self.assertLessEqual(unchecked.authored_bytes,LibrarianBudget().routing_prompt_bytes)
        self.assertGreater(contract._answer_bytes(snap["history"],snap["sources"]),4096)
        self.assertEqual(contract.prepare(current,rows,expected_db_id=DB),(None,"history_groups_overflow"))
        # An oversized group cannot erase a later, independent group that fits.
        other = binding();other["route"]["edge_fingerprint"] = "e"*64
        prompt,ctx = contract.prepare(current,rows+[witness(99,binding=other)],expected_db_id=DB)
        self.assertEqual(len(json.loads(ctx.snapshot_json)["history"]),1)
        self.assertEqual(json.loads(prompt)["omitted_history"],{"witnesses":31,"route_groups":1})

    def test_distinct_route_hint_pressure_is_not_witness_pressure(self):
        from routing_memory import MAX_HINT_BYTES,hint
        rows = self.tiny_history(35,distinct=True)
        current = [{"source":"task","text":"Inspect scope"}]
        prompt,ctx = contract.prepare(current,rows,expected_db_id=DB)
        snap = json.loads(ctx.snapshot_json)
        self.assertGreater(len(snap["history"]),8)
        self.assertLess(len(snap["history"]),len(rows))
        self.assertLessEqual(ctx.hint_bytes,MAX_HINT_BYTES)
        full = [hint(row["witness"]["binding"],"weaken") for row in rows]
        self.assertGreater(len(contract.encoded(full)),MAX_HINT_BYTES)
        self.assertEqual(len(contract.validate_answer(answer(ctx),ctx).hints),len(snap["bindings"]))
        self.assertEqual(json.loads(prompt)["omitted_history"]["route_groups"],1)

    def test_byte_pressure_omits_observed_opposing_group_without_positive_survivor(self):
        from librarian_policy import LibrarianBudget
        current = [{"source":"task", "text":"scope "+"x"*2500}]
        positive = witness(4,"boost",note="x"*2048)
        opposition = witness(5,"weaken",note="x"*2048)
        other_binding = binding(); other_binding["route"]["edge_fingerprint"] = "e"*64
        other = witness(6,binding=other_binding)
        low = LibrarianBudget(effort="low")
        self.assertIsNotNone(contract.prepare(current,[positive],expected_db_id=DB,budget=low)[0])
        _,full = contract.prepare(current,[positive,opposition],expected_db_id=DB)
        for verdict in ("matched","unknown"):
            self.assertEqual(contract.validate_answer(answer(full,["matched",verdict]),full).hints,())
        prompt,ctx = contract.prepare(current,[positive,opposition,other],expected_db_id=DB,budget=low)
        snap = json.loads(ctx.snapshot_json)
        self.assertEqual([r["id"] for r in snap["history"]],["w3"])
        self.assertEqual(json.loads(prompt)["omitted_history"],{"witnesses":2,"route_groups":1})
        out = contract.validate_answer(answer(ctx),ctx)
        self.assertEqual(out.hints,({**other_binding,"sign":"boost"},))
        self.assertNotIn(binding(),[{k:v for k,v in row.items() if k!="sign"} for row in out.hints])

    def test_worst_answer_accounts_every_current_ref_alias_and_bool(self):
        current = [{"source":f"source{i}","text":'scope "🐙" \\ '*20} for i in range(8)]
        prompt,ctx = contract.prepare(current,self.tiny_history(20),expected_db_id=DB)
        self.assertIsNotNone(prompt,ctx)
        worst = answer(ctx,["opposite"]*20)
        self.assertEqual(len(contract.encoded(worst)),ctx.worst_answer_bytes)
        self.assertLessEqual(ctx.worst_answer_bytes,ctx.answer_bytes)
        self.assertEqual(contract.validate_answer(worst,ctx).hints,())
        for row in worst["judgments"]:
            row["correction_applicable"] = True
        self.assertLess(len(contract.encoded(worst)),ctx.worst_answer_bytes)
        with self.assertRaises(ValueError):contract.validate_answer(worst,ctx)

    def test_provider_schema_subset_has_no_uniqueitems_and_host_rejects_duplicates(self):
        _,ctx = prepared([witness()])
        provider_schema = contract.schema(ctx)
        def walk(value):
            if isinstance(value,dict):
                self.assertNotIn("uniqueItems",value)
                for child in value.values():walk(child)
            elif isinstance(value,list):
                for child in value:walk(child)
        walk(provider_schema)
        row_schema = provider_schema["properties"]["judgments"]
        self.assertEqual(row_schema["minItems"],1)
        self.assertEqual(row_schema["maxItems"],1)
        self.assertEqual(row_schema["items"]["properties"]["current_refs"]["maxItems"],2)
        response = answer(ctx)
        response["judgments"][0]["current_refs"] = ["c1","c1"]
        with self.assertRaisesRegex(ValueError,"current_reference"):
            contract.validate_answer(response,ctx)
        # Allowing schema-level duplicates does not relax host byte accounting.
        self.assertLessEqual(ctx.worst_answer_bytes,ctx.answer_bytes)

    def test_schema_partitions_mixed_correction_eligibility_without_empty_branches(self):
        old = witness()
        new = witness(5,"weaken",corrects={"node_id":old["node_id"],"body_sha256":old["body_sha256"]})
        prompt,ctx = prepared([old,new])
        items = contract.schema(ctx)["properties"]["judgments"]["items"]
        branches = items["anyOf"]
        self.assertEqual(len(branches),2)
        by_handles = {tuple(branch["properties"]["witness"]["enum"]):branch for branch in branches}
        self.assertEqual(set(by_handles),{("w1",),("w2",)})
        self.assertEqual(by_handles[("w1",)]["properties"]["correction_applicable"],{"type":"boolean","enum":[False]})
        self.assertEqual(by_handles[("w2",)]["properties"]["correction_applicable"],{"type":"boolean"})
        self.assertTrue(all(branch["additionalProperties"] is False for branch in branches))
        self.assertNotIn("uniqueItems",json.dumps(items))
        self.assertEqual(ctx.authored_bytes,len(contract.INSTRUCTIONS.encode())+
                         len(contract.encoded(contract.schema(ctx)))+len(prompt.encode()))
        self.assertEqual(contract.validate_answer(answer(ctx,corrections=("w2",)),ctx).hints[0]["sign"],"weaken")
        for bad in (answer(ctx,corrections=("w1",)),answer(ctx,["matched","unknown"],("w2",))):
            with self.assertRaisesRegex(ValueError,"correction_reference"):
                contract.validate_answer(bad,ctx)

    def test_schema_missing_predecessor_forbids_correction_not_standalone_opinion(self):
        old = witness()
        missing = witness(5,"weaken",corrects={"node_id":old["node_id"],"body_sha256":old["body_sha256"]})
        _,ctx = prepared([missing])
        snap = json.loads(ctx.snapshot_json)
        self.assertIsNone(snap["history"][0]["corrects"])
        self.assertTrue(snap["history"][0]["correction_missing"])
        items = contract.schema(ctx)["properties"]["judgments"]["items"]
        self.assertNotIn("anyOf",items)
        self.assertEqual(items["properties"]["correction_applicable"]["enum"],[False])
        self.assertEqual(contract.validate_answer(answer(ctx),ctx).hints[0]["sign"],"weaken")
        with self.assertRaisesRegex(ValueError,"correction_reference"):
            contract.validate_answer(answer(ctx,corrections=("w1",)),ctx)
        malformed = answer(ctx);malformed["judgments"][0]["correction_applicable"] = 0
        with self.assertRaisesRegex(ValueError,"judgment_value"):
            contract.validate_answer(malformed,ctx)

    def test_schema_none_and_all_eligible_use_single_nonempty_item_shape(self):
        _,none = prepared([witness(),witness(5)])
        items = contract.schema(none)["properties"]["judgments"]["items"]
        self.assertNotIn("anyOf",items)
        self.assertEqual(items["properties"]["witness"]["enum"],["w1","w2"])
        self.assertEqual(items["properties"]["correction_applicable"]["enum"],[False])
        # A co-retrieved cycle is structurally eligible, not semantically resolved.
        a,b = witness(),witness(5,"weaken")
        a["witness"]["corrects"] = {"node_id":b["node_id"],"body_sha256":b["body_sha256"]}
        b["witness"]["corrects"] = {"node_id":a["node_id"],"body_sha256":a["body_sha256"]}
        _,all_eligible = prepared([a,b])
        items = contract.schema(all_eligible)["properties"]["judgments"]["items"]
        self.assertNotIn("anyOf",items)
        self.assertEqual(items["properties"]["correction_applicable"],{"type":"boolean"})
        out = contract.validate_answer(answer(all_eligible,corrections=("w1","w2")),all_eligible)
        self.assertEqual(out.hints,())
        self.assertEqual(out.decisions[0]["reason"],"unresolved_correction")

    def test_legacy_reason_is_rejected_and_current_work_unit_stays_separate(self):
        _,ctx = prepared([witness()])
        value = answer(ctx);value["judgments"][0]["reason"] = "Unused prose"
        with self.assertRaises(ValueError):contract.validate_answer(value,ctx)
        self.assertIsNone(contract.discovery_plan([{"source":f"s{i}","text":"scope"} for i in range(9)])[0])
        self.assertEqual(contract.discovery_plan([]),(None,"invalid_or_overflow_input"))
        self.assertIsNotNone(contract.prepare(CURRENT,[witness()]*9,expected_db_id=DB)[0])

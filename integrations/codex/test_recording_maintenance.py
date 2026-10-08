"""Optional concern maintenance alongside durable recording: synthetic, no native I/O."""
import copy
import hashlib
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import fixture_source_turn as source
import recording_contract as contract
import recording_jobs as jobs
from fixture_recording import DB_ID, SESSION, FakeRuntime, RecordingFixture


class RecordingMaintenanceTests(unittest.TestCase, RecordingFixture):
    def setUp(self):
        RecordingFixture.__init__(self, self)

    def maintenance_closed(self, *, registered=True, multiple=False):
        from test_recording_contract import concern_delivery
        self.begin()
        packet=concern_delivery(db_id=DB_ID,session=SESSION,turn="turn-1",registered=registered)["packet"]
        if multiple:
            extra=copy.deepcopy(packet["concerns"][0])
            extra["expected_row"]["notice"]["binding"]["key"]["kind"]="redundancy"
            extra["shown_text"]="Caveat: these could repeat only in this scope."
            packet["concerns"].append(extra)
            from turn_observer import render_delivery_concern
            packet["rendered_text"] += "\n" + render_delivery_concern(extra)
            packet["rendered_sha256"]=hashlib.sha256(packet["rendered_text"].encode()).hexdigest()
        self.assertEqual(jobs.attach_delivery(self.config,packet)["outcome"],"attached")
        self.append([source.row("response_item", {"type":"message","id":"hook-packet","role":"developer",
            "content":[{"type":"input_text","text":packet["rendered_text"]}],
            source.META:{"turn_id":"turn-1","content_item_kinds":["hooks.additional_context"]}}),
            source.call(turn="turn-1"),source.output(turn="turn-1",text="installed v1")])
        self.source_complete();jobs.close_turn(self.config,SESSION,"turn-1")
        return packet

    def maintenance_runtime(self, *, note=False, invalid_note=False, invalid_maintenance=False, defer=False, multiple=False):
        runtime=FakeRuntime(self)
        def assess(observation,overlap,*,timeout):
            runtime.calls.append((observation,overlap,timeout))
            _,context=contract.prepare(observation,overlap)
            evidence=[item.evidence_id for item in context.bindings if item.kind in ("memory_delivery","tool_result")]
            note_value={"kind":"lesson","summary":"A conditional lesson.","body":"The tool observed v1 in this deployment.","evidence_ids":[context.bindings[-2].evidence_id],"associate_with":None} if note else None
            if invalid_note:note_value={"kind":"invalid"}
            maintenance=[] if defer else [{"target":"not-issued" if invalid_maintenance else "case001", "scope":"This inspected deployment", "observation":"The tool reported installed v1; other deployments are unknown.","evidence":evidence}]
            if multiple and maintenance:
                maintenance = [{**maintenance[0],"target":target.opaque_id} for target in context.concern_bindings]
            value=contract.validate_answer({"proposal":note_value,"maintenance":maintenance},context)
            has_omissions=value["omissions"]["proposal"] or value["omissions"]["maintenance"]
            return {"reason":"proposed" if value["proposal"] or value["maintenance"] else "validation_failed" if has_omissions else "abstained",
                    "provider_attempt":True,"usage":{"input_tokens":20,"output_tokens":5},
                    "proposal":value["proposal"],"maintenance":value["maintenance"],"intent_omissions":value["omissions"]}
        runtime.assess=assess
        return runtime

    def maintained(self,config,job,item,timeout):
        self.assertEqual(len(self.accounts),1,"usage must settle before optional writes")
        self.assertEqual(self.state()["jobs"][0]["phase"],"write_intent")
        payload=item["payload"]
        self.assertEqual(payload["action"],"record_finding")
        return {"db":"project","db_id":DB_ID,"action":"record_finding","outcome":{
            "status":"applied","row":{**payload["expected"],"finding":payload["finding"]}}}

    def test_maintenance_only_settles_then_writes_one_checked_finding_without_save(self):
        packet=self.maintenance_closed();calls=[]
        def write(*args):calls.append(copy.deepcopy(args[2]));return self.maintained(*args)
        _,runtime=self.step(self.maintenance_runtime(),native_maintenance=write)
        self.assertEqual((len(runtime.calls),len(self.accounts),len(self.writes),len(calls)),(1,1,0,1))
        self.assertEqual(calls[0]["payload"]["expected"],packet["concerns"][0]["expected_row"])
        self.assertEqual(self.state()["jobs"],[])
        receipt=self.state()["receipts"][-1];self.assertNotIn("proposal",receipt)
        self.assertEqual(receipt["maintenance"][0]["status"],"applied")
        self.assertNotIn("native",receipt["maintenance"][0])
        self.assertTrue(receipt["maintenance"][0]["details_omitted"])

    def test_known_refusal_preserves_save_and_is_not_unresolved(self):
        self.maintenance_closed()
        def refuse(config,job,item,timeout):
            self.maintained(config,job,item,timeout)
            return {"db":"project","db_id":DB_ID,"action":"record_finding","outcome":{
                "status":"refused","reason":"stale_row","row":item["payload"]["expected"]}}
        self.step(self.maintenance_runtime(note=True),native_maintenance=refuse)
        self.assertEqual(len(self.writes),1);self.assertEqual(self.state()["jobs"],[])
        receipt=self.state()["receipts"][-1]
        self.assertEqual(receipt["save_outcome"]["status"],"verified")
        self.assertEqual(receipt["maintenance"][0]["status"],"refused")
        self.assertEqual(receipt["maintenance"][0]["reason"],"stale_row")

    def test_save_ambiguity_does_not_discard_finding_success_or_retry(self):
        self.maintenance_closed();calls=[]
        def fail(*args):self.writes.append(args);raise TimeoutError("lost acknowledgement")
        def maintain(*args):calls.append(args);return self.maintained(*args)
        self.step(self.maintenance_runtime(note=True),native_write=fail,native_maintenance=maintain)
        self.assertEqual((len(self.writes),len(calls)),(1,1));job=self.state()["jobs"][0]
        self.assertEqual(job["phase"],"unresolved");self.assertEqual(job["save_outcome"]["status"],"unresolved")
        self.assertEqual(job["maintenance"][0]["status"],"applied")
        partial=jobs.export_receipts(self.config,[SESSION]);self.assertTrue(partial["unknown"])
        self.assertEqual(partial["receipts"][0]["maintenance"][0]["status"],"applied")
        self.assertNotIn("payload",partial["receipts"][0]["maintenance"][0])
        self.assertFalse(self.step(native_write=fail,native_maintenance=maintain)[0])
        self.assertEqual((len(self.writes),len(calls)),(1,1))

    def test_finding_ambiguity_preserves_verified_save_and_exact_expected_row(self):
        packet=self.maintenance_closed();calls=[]
        def fail(config,job,item,timeout):calls.append(copy.deepcopy(item));raise TimeoutError("lost acknowledgement")
        self.step(self.maintenance_runtime(note=True),native_maintenance=fail)
        job=self.state()["jobs"][0];self.assertEqual(job["save_outcome"]["status"],"verified")
        self.assertEqual(job["maintenance"][0]["status"],"unresolved")
        self.assertEqual(job["maintenance"][0]["payload"]["expected"],packet["concerns"][0]["expected_row"])
        self.assertFalse(self.step(native_maintenance=fail)[0]);self.assertEqual(len(calls),1)

    def test_per_intent_validation_preserves_valid_siblings(self):
        self.maintenance_closed();self.step(self.maintenance_runtime(invalid_note=True),native_maintenance=self.maintained)
        receipt=self.state()["receipts"][-1]
        self.assertEqual(receipt["intent_omissions"]["proposal"],"invalid_proposal_shape")
        self.assertEqual(receipt["maintenance"][0]["status"],"applied");self.assertEqual(self.writes,[])

    def test_invalid_maintenance_preserves_note_and_explicit_empty_defer_writes_nothing(self):
        self.maintenance_closed();self.step(self.maintenance_runtime(note=True,invalid_maintenance=True),native_maintenance=self.maintained)
        self.assertEqual(len(self.writes),1)
        self.assertEqual(self.state()["receipts"][-1]["intent_omissions"]["maintenance"],{"maintenance_target":1})

    def test_explicit_empty_maintenance_is_abstention_not_rejection(self):
        self.maintenance_closed();self.step(self.maintenance_runtime(defer=True),native_maintenance=self.maintained)
        self.assertEqual(self.writes,[]);self.assertEqual(self.state()["counts"]["abstained"],1)
        self.assertEqual(self.state()["receipts"][-1]["intent_omissions"],{"proposal":None,"maintenance":{}})

    def test_accounting_failure_blocks_save_and_maintenance(self):
        self.maintenance_closed();calls=[]
        self.step(self.maintenance_runtime(note=True),account=lambda *_:False,native_maintenance=lambda *args:calls.append(args))
        self.assertEqual(self.writes,[]);self.assertEqual(calls,[])
        self.assertEqual(self.state()["jobs"][0]["reason"],"accounting_unavailable")

    def test_cancellation_between_branches_keeps_save_outcome_and_blocks_finding(self):
        self.maintenance_closed();calls=[]
        def save(*args):
            result=self.native_write(*args);jobs.close_turn(self.config,SESSION,cancel=True);return result
        self.step(self.maintenance_runtime(note=True),native_write=save,native_maintenance=lambda *args:calls.append(args))
        self.assertEqual(calls,[]);self.assertEqual(len(self.writes),1)
        receipt=self.state()["receipts"][-1];self.assertEqual(receipt["save_outcome"]["status"],"verified")
        self.assertEqual(receipt["maintenance"][0]["status"],"pending")
        self.assertEqual(receipt["outcome"],"cancelled")

    def test_oversized_optional_findings_are_omitted_without_losing_note(self):
        self.maintenance_closed();calls=[]
        # Size from the complete current delivery snapshot, not legacy v1's
        # smaller packet. Allow entry but leave no spare finding/receipt reserve.
        entry_bytes = len(jobs.encoded(self.state()["jobs"][0]))
        with patch.object(jobs,"MAX_JOB_BYTES",entry_bytes + 256):
            self.step(self.maintenance_runtime(note=True),native_maintenance=lambda *args:calls.append(args))
        self.assertEqual(len(self.writes),1);self.assertEqual(calls,[])
        receipt=self.state()["receipts"][-1];self.assertEqual(receipt["maintenance_omitted_count"],1)
        self.assertEqual(receipt["outcome"],"verified")

    def test_checked_bridge_uses_one_call_guard_and_no_refresh(self):
        job={"config_sha256":"binding","db_id":DB_ID};item={"payload":{"action":"record_finding","expected":{},"finding":{}}}
        with patch("service.load_config",return_value=SimpleNamespace(token_env="",url="http://127.0.0.1:12345/")), patch("recording_jobs._config_digest",return_value="binding"),patch("mcp_client.McpClient") as factory:
            client=factory.return_value;client.concern_checked.return_value={"sent":True}
            self.assertEqual(jobs._native_maintenance(self.config,job,item,2),{"sent":True})
            client.concern_checked.assert_called_once_with("project",item["payload"],expected_db_id=DB_ID)
            client.call_tool.assert_not_called();client.close.assert_called_once()

    def test_multiple_findings_have_independent_outcomes_after_one_assessment(self):
        self.maintenance_closed(multiple=True);calls=[]
        def write(config,job,item,timeout):
            calls.append(item["target"])
            if item["target"]=="case001":raise TimeoutError("lost ack")
            return self.maintained(config,job,item,timeout)
        self.step(self.maintenance_runtime(multiple=True),native_maintenance=write)
        self.assertEqual(calls,["case001","case002"])
        self.assertEqual(self.writes,[]);self.assertEqual(len(self.accounts),1)
        items=self.state()["jobs"][0]["maintenance"]
        self.assertEqual([item["status"] for item in items],["unresolved","applied"])
        self.assertFalse(self.step(native_maintenance=write)[0])

    def test_exact_unchanged_finding_reply_is_verified_without_readback(self):
        self.maintenance_closed();calls=[]
        def write(*args):
            calls.append(copy.deepcopy(args[2]["payload"]))
            result=self.maintained(*args);result["outcome"]["status"]="unchanged";return result
        self.step(self.maintenance_runtime(),native_maintenance=write)
        self.assertEqual(len(calls),1);self.assertEqual(self.state()["jobs"],[])
        self.assertEqual(self.state()["receipts"][-1]["maintenance"][0]["status"],"unchanged")

    def test_config_change_between_branches_preserves_save_without_finding_call(self):
        self.maintenance_closed();calls=[]
        def save(*args):
            result=self.native_write(*args);self.service.write_text('{"changed":true}');return result
        self.step(self.maintenance_runtime(note=True),native_write=save,native_maintenance=lambda *args:calls.append(args))
        self.assertEqual(calls,[])
        receipt=self.state()["receipts"][-1]
        self.assertEqual(receipt["save_outcome"]["status"],"verified")
        self.assertEqual(receipt["maintenance"][0]["status"],"pending")
        self.assertEqual(receipt["outcome"],"deferred")

    def test_large_known_refusal_remains_terminal_with_explicit_detail_omission(self):
        self.maintenance_closed()
        def refuse(config,job,item,timeout):
            self.maintained(config,job,item,timeout)
            row=copy.deepcopy(item["payload"]["expected"])
            row["finding"]={"scope":"\0"*512,"observation":"\0"*1024,
                "evidence":[{"source_ref":"\0"*256,"digest":"c"*64}]}
            # Reduce only retained detail room after frozen source admission.
            current_size=len(jobs.encoded(self.state()["jobs"][0]))
            cap=patch.object(jobs,"MAX_JOB_BYTES",current_size+500)
            cap.start();self.addCleanup(cap.stop)
            return {"db":"project","db_id":DB_ID,"action":"record_finding","outcome":{
                "status":"refused","reason":"stale_row","row":row}}
        self.step(self.maintenance_runtime(),native_maintenance=refuse)
        self.assertEqual(self.state()["jobs"],[])
        receipt=self.state()["receipts"][-1]
        self.assertEqual(receipt["maintenance"][0]["status"],"refused")
        self.assertTrue(receipt["maintenance"][0]["details_omitted"])
        self.assertEqual(receipt["outcome"],"deferred")
        self.assertEqual(self.state()["counts"]["unresolved"],0)



if __name__ == "__main__":
    unittest.main()

"""Disposable auxiliary stores and injected owners/models; no live calls."""
import json
from contextlib import closing
from pathlib import Path
import sqlite3
import tempfile
import time
import unittest
from unittest.mock import patch

import stewardship as subject
from librarian_policy import LibrarianBudget
from tag_context import TagContext

from fixture_stewardship import DB, A, B, C, FP, GUIDE, context, target

class JournalTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name)/"journal"
        self.budget=LibrarianBudget()

    def journal(self,**kwargs):
        return subject.Journal(DB,root=self.root,now=100000,**kwargs)

    def test_absent_inspection_does_not_create_state(self):
        self.assertEqual(subject.inspect(DB,root=self.root)["outcome"],"absent")
        self.assertFalse(self.root.exists())

    def test_reopen_lease_and_identity(self):
        with self.journal() as journal:
            journal.enqueue([A]);journal.page([B],"continuation")
            with self.assertRaises(BlockingIOError):self.journal()
        with self.journal() as journal:
            self.assertEqual(journal.get("cursor"),"continuation")
            self.assertEqual(journal.candidates(GUIDE,2),[B,A])
        self.assertEqual(subject.inspect(DB,root=self.root)["outcome"],"available")

    def test_unknown_and_corrupt_are_not_reinitialized(self):
        self.root.mkdir()
        path=self.root/(DB+".sqlite3")
        for blob in (b"not sqlite",b""):
            path.write_bytes(blob)
            with self.assertRaises((ValueError,sqlite3.Error)):self.journal()
            self.assertEqual(path.read_bytes(),blob)
        path.unlink()
        with closing(sqlite3.connect(path)) as db:db.execute("CREATE TABLE precious(value TEXT)")
        before=path.read_bytes()
        with self.assertRaises(ValueError):self.journal()
        self.assertEqual(path.read_bytes(),before)

    def test_wrong_owner_and_hardlink_fail_without_reset(self):
        with self.journal() as journal:journal.put("owner",B)
        before=(self.root/(DB+".sqlite3")).read_bytes()
        with self.assertRaises(ValueError):self.journal()
        self.assertEqual((self.root/(DB+".sqlite3")).read_bytes(),before)
        (self.root/(DB+".sqlite3")).unlink()
        with self.journal():pass
        import os
        os.link(self.root/(DB+".sqlite3"),self.root/"alias")
        with self.assertRaises(ValueError):self.journal()

    def test_symlink_aliases_refuse_and_current_inspection_is_read_only(self):
        with self.journal() as journal:
            journal.enqueue([A])
            before=journal.path.read_bytes()
            self.assertEqual(subject.inspect(DB,root=self.root)["outcome"],"available")
            self.assertEqual(journal.path.read_bytes(),before)
        alias=self.root.parent/"alias";alias.symlink_to(self.root,target_is_directory=True)
        with self.assertRaises(ValueError):subject.Journal(DB,root=alias)
        (self.root/(B+".sqlite3")).symlink_to(self.root/(DB+".sqlite3"))
        with self.assertRaises(ValueError):subject.Journal(B,root=self.root)

    def test_cold_share_dirty_and_policy_cursor_are_separate(self):
        with self.journal() as journal:
            journal.page([A,B],"cold-progress")
            journal.enqueue([C]);journal.mark(A,FP,GUIDE,"noop")
            journal.mark(B,FP,GUIDE,"unresolved")
            self.assertTrue(journal.examined(B,FP,GUIDE))
            journal.candidates("c"*64,2)
            self.assertEqual(journal.get("cursor"),"cold-progress")
            self.assertFalse(journal.examined(B,FP,"c"*64))
            journal.page([A,B],None)
            selected=journal.candidates("c"*64,2)
            self.assertEqual(selected[0],A)
            self.assertEqual(len(selected),2)

    def test_finished_cold_sweep_has_cooldown_not_restart(self):
        with self.journal() as journal:
            journal.page([A],None)
            journal.mark(A,FP,GUIDE,"unresolved")
            self.assertFalse(journal.needs_page())
            self.assertEqual(journal.candidates(GUIDE,4),[])

    def test_unknown_spend_survives_reopen_and_shared_session(self):
        with self.journal() as journal:
            token=journal.reserve([target()],GUIDE,self.budget)
            self.assertIsNotNone(token)
        with self.journal() as journal:
            journal.recover(self.budget)
            self.assertFalse(journal.examined(A,FP,GUIDE))
            self.assertIsNone(journal.reserve([target(B)],GUIDE,self.budget))
            row=journal.db.execute("SELECT * FROM usage").fetchone()
            self.assertEqual(row["unknown"],1)
            self.assertEqual(row["input"],self.budget.input_tokens)

    def test_known_usage_and_native_rounds_are_owner_shared(self):
        with self.journal() as journal:
            self.assertTrue(journal.reserve_native(self.budget))
            self.assertFalse(journal.reserve_native(self.budget))
            token=journal.reserve([target()],GUIDE,self.budget)
            self.assertTrue(journal.settle(token,{"provider_attempt":True,"usage":{"input_tokens":700,"output_tokens":50}},self.budget))
        with self.journal() as journal:
            row=journal.db.execute("SELECT * FROM usage").fetchone()
            self.assertEqual((row["attempts"],row["input"],row["output"],row["native_rounds"]),(1,700,50,1))

    def test_unknown_ack_is_history_not_reconstructed_success(self):
        with self.journal() as journal:
            seq=journal.intent(target(),GUIDE,["rust","people"])
        with self.journal() as journal:
            journal.recover(self.budget)
            self.assertEqual(journal.db.execute("SELECT status FROM intents WHERE seq=?",(seq,)).fetchone()[0],"unknown")
            self.assertFalse(journal.examined(A,FP,GUIDE))
            self.assertTrue(journal.deferred(A,FP,GUIDE))
            # Fresh content can be assessed, but this never updates old history.
            self.assertFalse(journal.examined(A,"c"*64,GUIDE))
            journal.mark(A,"c"*64,GUIDE,"noop")
            self.assertEqual(subject.inspect(DB,root=self.root)["actions"][0]["status"],"unknown")


class FakeOwner:
    ids=[A]
    writes=[]
    lost=False
    changed=False
    def __init__(self,*_):
        self.db_id=DB;self.deadline=time.monotonic()+15;self.remaining_bytes=64000
    def __enter__(self):return self
    def __exit__(self,*_):pass
    def authorize(self):pass
    def page(self,cursor,limit):return self.ids[:limit],None
    def target(self,identifier):
        self.remaining_bytes-=256
        fp="c"*64 if self.changed else FP
        return target(identifier,fp),fp
    def retag(self,node,tags,guards):
        self.writes.append((node,tags,guards))
        if self.lost:raise TimeoutError()
        return {"db_id":DB,"id":node["id"],"tags":tags,"changed":tags!=node["tags"]}


class FakeRuntime:
    calls=0
    mode="noop"
    def steward(self,targets,context,**_):
        self.calls+=1
        if self.mode=="unknown":return {"provider_attempt":True,"usage":None,"decisions":None}
        if self.mode=="crash":raise RuntimeError()
        return {"provider_attempt":True,"usage":{"input_tokens":100,"output_tokens":10},
                "decisions":[{"id":t["id"],"disposition":self.mode,
                    "tags":["rust","people"] if self.mode=="retag" else t["tags"]} for t in targets]}


class StepTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name)
        self.config={"tag_stewardship":True,"recording_mode":"automatic","memory_mode":"async",
            "reader_model":"gpt-6.1-sol","librarian_effort":"medium"}
        self.runtime=FakeRuntime();self.accounts=[]
        FakeOwner.writes=[];FakeOwner.lost=False;FakeOwner.changed=False;FakeOwner.ids=[A]
        for name,value in (("journal_root",lambda:self.root),("NativeOwner",FakeOwner)):
            p=patch.object(subject,name,value);p.start();self.addCleanup(p.stop)
        p=patch("tag_context.collect",return_value=context());p.start();self.addCleanup(p.stop)

    def step(self,allowed=lambda:True,reserve=lambda _:True):
        return subject.step(self.config,"session",self.runtime,reserve,
            lambda key,result:(self.accounts.append((key,result)) or True),allowed=allowed)

    def resume(self,identifier=A):
        with subject.Journal(DB) as journal:
            journal.put("next_batch",0)
            journal.enqueue([identifier])

    def test_disabled_means_no_native_journal_or_model(self):
        self.config["recording_mode"]="off"
        with patch.object(subject,"NativeOwner") as native:
            self.assertFalse(self.step());native.assert_not_called()
        self.assertEqual(list(self.root.iterdir()),[])
        self.assertEqual(self.runtime.calls,0)

    def test_noop_and_unresolved_are_stable_without_repeated_model(self):
        for mode in ("noop","unresolved"):
            self.runtime.mode=mode
            self.assertTrue(self.step())
            calls=self.runtime.calls
            self.resume()
            self.assertFalse(self.step())
            self.assertEqual(self.runtime.calls,calls)
            with subject.Journal(DB) as journal:
                journal.db.execute("DELETE FROM work");journal.put("next_batch",0);journal.put("next_sweep",0)

    def test_retag_guarded_once_and_lost_ack_never_blind_replay(self):
        self.runtime.mode="retag";FakeOwner.lost=True
        self.assertTrue(self.step());self.assertEqual(len(FakeOwner.writes),1)
        self.assertEqual(subject.inspect(DB)["actions"][0]["status"],"unknown")
        self.resume();self.assertFalse(self.step());self.assertEqual(len(FakeOwner.writes),1)
        FakeOwner.changed=True;self.resume();self.assertTrue(self.step())
        self.assertEqual(len(FakeOwner.writes),2)
        self.assertEqual(subject.inspect(DB)["actions"][-1]["status"],"unknown")

    def test_checked_ack_is_success(self):
        self.runtime.mode="retag"
        self.assertTrue(self.step())
        self.assertEqual(subject.inspect(DB)["actions"][0]["status"],"applied")

    def test_provider_unknown_is_charged_and_not_repeated(self):
        self.runtime.mode="crash"
        self.assertTrue(self.step())
        self.assertEqual(len(self.accounts),1)
        self.assertEqual(subject.inspect(DB)["usage"]["unknown"],1)
        self.resume();self.assertFalse(self.step());self.assertEqual(self.runtime.calls,1)

    def test_foreground_priority_and_failed_session_admission(self):
        self.assertFalse(self.step(allowed=lambda:False));self.assertEqual(self.runtime.calls,0)
        self.assertFalse(self.step(reserve=lambda _:False));self.assertEqual(self.runtime.calls,0)

    def test_same_state_unknown_ack_reassesses_later_without_rewriting_history(self):
        self.runtime.mode="retag";FakeOwner.lost=True
        self.assertTrue(self.step())
        with subject.Journal(DB) as journal:
            journal.put("next_batch",0)
            journal.db.execute("UPDATE work SET retry_after=1")
        self.runtime.mode="noop";FakeOwner.lost=False
        self.assertTrue(self.step())
        self.assertEqual(self.runtime.calls,2)
        self.assertEqual(len(FakeOwner.writes),1)
        self.assertEqual(subject.inspect(DB)["actions"][0]["status"],"unknown")

    def test_invalid_cold_head_defers_and_next_candidate_progresses(self):
        FakeOwner.ids=[A,B]
        original=FakeOwner.target
        def get(owner,identifier):
            if identifier==A:raise ValueError("deleted or malformed")
            return original(owner,identifier)
        with patch.object(FakeOwner,"target",get):self.assertTrue(self.step())
        with subject.Journal(DB) as journal:
            self.assertEqual(journal.db.execute("SELECT outcome FROM work WHERE id=?",(A,)).fetchone()[0],"read_deferred")
            self.assertEqual(journal.db.execute("SELECT outcome FROM work WHERE id=?",(B,)).fetchone()[0],"noop")

    def test_resource_defer_is_not_semantic_abstention_and_effort_can_reopen(self):
        original=FakeOwner.target
        def get(owner,identifier):
            node,fp=original(owner,identifier);node["summary"]="detail "*1600
            return node,fp
        self.config["librarian_effort"]="low"
        with patch.object(FakeOwner,"target",get):
            self.assertFalse(self.step());self.assertEqual(self.runtime.calls,0)
            with subject.Journal(DB) as journal:
                row=journal.db.execute("SELECT * FROM work WHERE id=?",(A,)).fetchone()
                self.assertEqual(row["outcome"],"resource_deferred")
                self.assertFalse(journal.examined(A,FP,row["guide"]))
            self.config["librarian_effort"]="high";self.resume()
            self.assertTrue(self.step());self.assertEqual(self.runtime.calls,1)


class NativeOwnerTests(unittest.TestCase):
    def test_real_host_adapter_uses_complete_summary_and_strong_owner_guards(self):
        from types import SimpleNamespace
        from target_policy import TargetPolicy
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary);service_path=root/"service.json";service_path.write_text("{}")
            database=root/".mneme/codex-memory.db"
            policy=TargetPolicy("project","project",str(database),None,str(root),str(service_path))
            service=SimpleNamespace(url="http://127.0.0.1:12345/",token_env=None)
            calls=[]
            class Client:
                timeout=1
                def connect(self):return self
                def close(self):pass
                def owner_capabilities(self):return {"retag_content_guards":True,"tag_vocabulary":True}
                def call_tool(self,name,args):
                    calls.append((name,args))
                    if name=="databases":return [{"db":"project","name":"project","state":"open","configured_path":str(database),"db_id":DB}]
                    if name=="get":return {**target(),"db_id":DB,"status":"active","memory_kind":{"kind":"semantic"},
                        "summary":"s"*4096,"summary_truncated":False,"content_fingerprint_codec":subject.CODEC}
                    if name=="retag":return {"db_id":DB,"id":A,"tags":args["tags"],"changed":True}
            config={"service_config":service_path,"project_root":root}
            with patch("target_policy.policy_for",return_value=(policy,service)),patch("mcp_client.McpClient",return_value=Client()):
                with subject.NativeOwner(config,LibrarianBudget()) as owner:
                    owner.authorize();node,fp=owner.target(A)
                    self.assertEqual(len(node["summary"]),4096)
                    receipt=owner.retag(node,["rust","people"],[{"id":B,"content_fingerprint":GUIDE}])
                    self.assertEqual(subject._acknowledgement(receipt,DB,node,["people","rust"]),"applied")
            self.assertEqual(calls[1],("get",{"db":"project","expected_db_id":DB,"id":A,"body":False}))
            self.assertEqual(calls[2][1]["expected_content_fingerprint"],FP)
            self.assertEqual(calls[2][1]["guard_nodes"],[{"id":B,"content_fingerprint":GUIDE}])

    def test_old_or_malformed_owner_capabilities_fail_without_catalog_fallback(self):
        owner=object.__new__(subject.NativeOwner);owner.remaining_bytes=4096
        from unittest.mock import Mock
        owner.client=Mock()
        for result in ({},{"retag_content_guards":1,"tag_vocabulary":True}):
            owner.client.owner_capabilities.return_value=result
            with self.assertRaises(ValueError):owner.authorize()
        owner.client.list_tools.assert_not_called()


if __name__=="__main__":unittest.main()

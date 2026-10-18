"""Synthetic official-SDK-shaped JSONL. Not a real installed Claude receipt."""
from dataclasses import asdict, replace
import json
from pathlib import Path
import sys
import tempfile
import unittest
sys.path.insert(0, str(Path(__file__).parent))
import claude_observer as observer
from rollout_primitives import digest


class ClaudeObserverTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.path = self.root / 'session.jsonl'
        self.path.write_bytes(b'')
        self.prompt = 'Keep the exact source provenance.'

    def row(self, kind, identifier, parent, content, **kw):
        return {'type':kind, 'uuid':identifier, 'parentUuid':parent, 'sessionId':'session',
                'isSidechain':False, 'message':{'role':kind, 'content':content}, **kw}

    def user(self):
        return self.row('user','u',None,self.prompt)

    def assistant(self, text='Done.'):
        return self.row('assistant','a','u',[{'type':'text','text':text}])

    def write(self, rows):
        self.path.write_text(''.join(json.dumps(row)+'\n' for row in rows))

    def admit(self):
        return observer.admit_source_turn(self.path, sessions_root=self.root,session_id='session',turn_id='prompt',
                                          expected_prompt_sha256=digest(self.prompt.encode()))

    def receipt(self, text='Done.'):
        return {'hook_event_name':'Stop','session_id':'session','prompt_id':'prompt',
                'transcript_path':str(self.path),'last_assistant_message':text,'stop_hook_active':False}

    def boundary(self, text='Next task.', *, kind='next_prompt'):
        return {'schema':observer.BOUNDARY_SCHEMA,'session_id':'session','turn_id':'prompt','kind':kind,
                'next_turn_id':'next-prompt' if kind=='next_prompt' else None,
                'prompt_sha256':digest(text.encode()) if kind=='next_prompt' else None,
                'transcript_path':str(self.path)}

    def seal(self):
        """Append a concrete synthetic next-prompt marker, not an EOF fiction."""
        rows=[json.loads(line) for line in self.path.read_text().splitlines()]
        last=next(r for r in reversed(rows) if not observer._excluded(r) and r.get('type') in ('user','assistant'))
        if last.get('type')=='user' and observer._prompt(last) != self.prompt:
            return self.boundary(observer._prompt(last))
        marker=self.row('user','next','%s'%last['uuid'],'Next task.')
        with self.path.open('a') as stream:stream.write(json.dumps(marker)+'\n')
        return self.boundary()

    def observe(self, admission, **kw):
        return observer.observe_source_turn(admission, **kw)

    def test_preflush_prompt_and_stop_need_actual_final_transcript(self):
        admission = self.admit()['admission']
        self.assertEqual(self.observe(admission)['reason'],'source_prompt_pending')
        self.write([self.user()])
        self.assertEqual(self.observe(admission,stop_receipt=self.receipt())['reason'],'source_boundary_pending')
        self.write([self.user(),self.assistant()])
        result = self.observe(admission,stop_receipt=self.receipt(),boundary_receipt=self.seal())
        self.assertEqual(result['status'],'observed', result)
        self.assertEqual(result['boundary'],result['evidence'][-1]['ref'])
        self.assertEqual(result['coverage']['prompt_authority'],'unverified')
        self.assertEqual(result['coverage']['memory_delivery'],'not_recorded')
        self.assertEqual(result['evidence'][0]['source_field'],'/message/content')

    def test_eof_and_final_without_stop_are_not_completion(self):
        self.write([self.user()]); a=self.admit()['admission']
        self.write([self.user(),self.assistant()])
        self.assertEqual(self.observe(a)['reason'],'source_turn_not_closed')

    def test_two_blocks_one_line_preserve_physical_anchor_and_logical_order(self):
        self.write([self.user()]); a=self.admit()['admission']
        assistant=self.assistant(); assistant['message']['content']=[{'type':'thinking','thinking':'SECRET'},
            {'type':'text','text':'One.'},{'type':'text','text':'Two.'}]
        self.write([self.user(),assistant])
        result=self.observe(a,stop_receipt=self.receipt('One.\nTwo.'),boundary_receipt=self.seal())
        self.assertEqual(result['status'],'observed',result)
        items=result['evidence'][1:]
        self.assertEqual(items[0]['ref'],items[1]['ref'])
        self.assertEqual(items[0]['ref']['ordinal'],1)
        self.assertEqual([v['evidence_index'] for v in items],[1,2])
        self.assertEqual(items[1]['source_field'],'/message/content/2/text')
        self.assertNotIn('SECRET',json.dumps(result))
        raw=self.path.read_bytes().splitlines(keepends=True)[1]
        self.assertEqual(items[0]['ref']['raw_line_sha256'],digest(raw))
        self.assertEqual(items[0]['ref']['line_bytes'],len(raw))

    def test_tool_results_are_not_user_statements_and_calls_preserve_json(self):
        self.write([self.user()]); a=self.admit()['admission']
        call=self.row('assistant','call','u',[{'type':'tool_use','id':'t','name':'Read','input':{'file_path':'synthetic'}}])
        output=self.row('user','result','call',[{'type':'tool_result','tool_use_id':'t','content':'synthetic output'}])
        final=self.assistant(); final['parentUuid']='result'
        self.write([self.user(),call,output,final])
        result=self.observe(a,stop_receipt=self.receipt(),boundary_receipt=self.seal())
        self.assertEqual(result['status'],'observed',result)
        self.assertEqual([v['kind'] for v in result['evidence']],['user_statement','tool_call','tool_result','assistant_assertion'])
        self.assertEqual(result['evidence'][1]['input'],{'file_path':'synthetic'})
        self.assertEqual(result['evidence'][2]['source_field'],'/message/content/0/content')

    def test_interruption_branch_change_missing_result_and_bad_final_defer(self):
        for variant in ('branch','interrupt','missing','mismatch'):
            with self.subTest(variant=variant):
                self.write([self.user()]); a=self.admit()['admission']
                final=self.assistant(); rows=[self.user()]
                if variant=='branch': final['parentUuid']='other'
                if variant=='interrupt':
                    rows.append(self.row('user','new','u','New task.')); final['parentUuid']='new'
                if variant=='missing':
                    rows.append(self.row('assistant','call','u',[{'type':'tool_use','id':'t','name':'Read','input':{}}])); final['parentUuid']='call'
                rows.append(final); self.write(rows)
                result=self.observe(a,stop_receipt=self.receipt('Other.' if variant=='mismatch' else 'Done.'))
                self.assertEqual(result['status'],'deferred',result)
                self.assertEqual(result['evidence'],[])

    def test_sidechain_and_agent_type_root(self):
        self.write([self.user()]); a=self.admit()['admission']
        child=self.row('assistant','child','u','CHILD',isSidechain=True)
        final=self.assistant();final['agent_type']='reviewer'
        self.write([self.user(),child,final])
        result=self.observe(a,stop_receipt=self.receipt(),boundary_receipt=self.seal())
        self.assertEqual(result['status'],'observed',result)
        self.assertNotIn('CHILD',json.dumps(result))

    def test_large_prior_history_uses_bounded_tail_and_small_anchored_turn(self):
        history=[]
        for n in range(450):
            history.append(self.row('user',f'old{n}',None,'Earlier text. '+'x'*2048))
        self.write(history)
        a=self.admit()['admission']
        self.assertGreater(a.start_offset,256*1024)
        self.assertFalse(observer.first_turn(a))
        with self.path.open('a') as stream:
            current=self.user();current['parentUuid']='old449'
            stream.write(json.dumps(current)+'\n')
        pending=self.observe(a,limits=observer.Limits(scan_bytes=256*1024,events=512),stop_receipt=self.receipt())
        self.assertEqual(pending['reason'],'source_boundary_pending')
        with self.path.open('a') as stream:
            stream.write(json.dumps(self.assistant())+'\n')
        result=self.observe(a,limits=observer.Limits(scan_bytes=256*1024,events=512),stop_receipt=self.receipt(),boundary_receipt=self.seal())
        self.assertEqual(result['status'],'observed',result)
        self.assertEqual(result['evidence'][0]['ref']['ordinal_scope'],'source_window')
        self.assertEqual(result['evidence'][0]['ref']['ordinal'],0)
        self.assertEqual(result['evidence'][0]['ref']['byte_offset'],a.start_offset)
        self.assertLess(result['work']['bytes_read'],16*1024)
        self.assertEqual(observer.snapshot_end(a,result),self.path.stat().st_size)

    def test_first_prompt_after_metadata_is_first_turn(self):
        metadata={'type':'file-history-snapshot','snapshot':{'messageId':'synthetic'}}
        self.write([metadata,self.user()])
        admission=self.admit()['admission']
        self.assertGreater(admission.start_offset,0)
        self.assertTrue(observer.first_turn(admission))
        self.write([metadata,self.user(),self.assistant()])
        self.assertEqual(self.observe(admission,stop_receipt=self.receipt(),boundary_receipt=self.seal())['status'],'observed')

    def test_flushed_prompt_raw_anchor_change_rejects_same_text_new_identity(self):
        self.write([self.user()]);admission=self.admit()['admission']
        changed=self.user();changed['uuid']='other'
        self.write([changed,self.assistant()])
        self.assertEqual(self.observe(admission,stop_receipt=self.receipt())['reason'],'source_prompt_anchor_changed')

    def test_receipt_bound_completed_prefix_survives_next_prompt(self):
        self.write([self.user()]);admission=self.admit()['admission']
        next_prompt=self.row('user','next','a','Next task.')
        self.write([self.user(),self.assistant(),next_prompt])
        result=self.observe(admission,stop_receipt=self.receipt(),boundary_receipt=self.seal())
        self.assertEqual(result['status'],'observed',result)
        self.assertEqual(result['boundary']['item_id'],'a')
        self.assertNotIn('Next task.',json.dumps(result))

    def test_prior_final_flush_before_current_prompt_is_omitted_not_current_evidence(self):
        old=self.row('user','old',None,'Previous task.')
        self.write([old]);admission=self.admit()['admission']
        previous_final=self.row('assistant','prior_final','old','Previous final.')
        current=self.user();current['parentUuid']='prior_final'
        self.write([old,previous_final,current,self.assistant()])
        result=self.observe(admission,stop_receipt=self.receipt(),boundary_receipt=self.seal())
        self.assertEqual(result['status'],'observed',result)
        self.assertEqual(result['evidence'][0]['ref']['ordinal'],1)
        self.assertEqual(result['coverage']['omissions']['other_host_message'],1)
        self.assertNotIn('Previous final.',json.dumps(result))

    def test_earlier_identical_commentary_is_not_borrowed_as_final_boundary(self):
        self.write([self.user()]);admission=self.admit()['admission']
        commentary=self.row('assistant','comment','u','Done.')
        actual=self.row('assistant','actual','comment','Still working.')
        self.write([self.user(),commentary,actual,self.row('user','next','actual','Next task.')])
        self.assertEqual(self.observe(admission,stop_receipt=self.receipt())['status'],'deferred')

    def test_physical_sidechain_anchor_is_not_logical_main_parent(self):
        old=self.row('user','old',None,'Previous task.')
        old_final=self.row('assistant','old_final','old','Previous final.')
        child=self.row('assistant','child','old','Child output.',isSidechain=True)
        self.write([old,old_final,child]);admission=self.admit()['admission']
        self.assertEqual(admission.preceding_anchor['item_id'],'child')
        self.assertEqual(admission.logical_preceding_anchor['item_id'],'old_final')
        current=self.user();current['parentUuid']='old_final'
        self.write([old,old_final,child,current,self.assistant()])
        result=self.observe(admission,stop_receipt=self.receipt(),boundary_receipt=self.seal())
        self.assertEqual(result['status'],'observed',result)
        self.assertNotIn('Child output.',json.dumps(result))

    def test_closed_prefix_survives_large_or_malformed_later_turn(self):
        self.write([self.user()]);admission=self.admit()['admission']
        next_prompt=self.row('user','next','a','Next task.')
        for suffix in (b'{malformed later record}\n',b'partial later',b'x'*4096):
            self.write([self.user(),self.assistant(),next_prompt])
            ending=self.seal()
            with self.path.open('ab') as stream:stream.write(suffix)
            result=self.observe(admission,limits=observer.Limits(scan_bytes=1024,line_bytes=1024),stop_receipt=self.receipt(),boundary_receipt=ending)
            self.assertEqual(result['status'],'observed',result)
            self.assertEqual(result['boundary']['item_id'],'a')
            self.assertLessEqual(result['work']['bytes_read'],observer.observation_byte_ceiling(admission,limits=observer.Limits(scan_bytes=1024,line_bytes=1024)))

    def test_retained_compact_summary_does_not_mint_fresh_first_turn(self):
        summary=self.row('user','summary',None,'Prior work summary.',isCompactSummary=True)
        self.write([summary])
        admission=self.admit()['admission']
        self.assertFalse(observer.first_turn(admission))

    def test_stop_candidate_is_not_completion_without_later_trusted_raw_boundary(self):
        self.write([self.user()]);admission=self.admit()['admission']
        self.write([self.user(),self.assistant()])
        self.assertEqual(self.observe(admission,stop_receipt=self.receipt())['reason'],'source_boundary_pending')
        self.assertEqual(self.observe(admission,stop_receipt=self.receipt(),boundary_receipt=self.boundary())['reason'],'source_boundary_pending')
        result=self.observe(admission,stop_receipt=self.receipt(),boundary_receipt=self.boundary(kind='session_end'))
        self.assertEqual(result['reason'],'source_snapshot_ready')
        self.assertEqual(result['coverage']['turn_finality'],'unproven')
        self.assertEqual(result['coverage']['remaining_transcript'],'unknown')

    def test_latest_stop_candidate_must_match_continued_prompt_before_boundary(self):
        self.write([self.user()]);admission=self.admit()['admission']
        interim=self.assistant('Interim.')
        final=self.row('assistant','continued','a','Actual final.')
        self.write([self.user(),interim,final]);ending=self.seal()
        self.assertEqual(self.observe(admission,stop_receipt=self.receipt('Interim.'),boundary_receipt=ending)['status'],'deferred')
        result=self.observe(admission,stop_receipt=self.receipt('Actual final.'),boundary_receipt=ending)
        self.assertEqual(result['status'],'observed',result)
        self.assertEqual(result['boundary']['item_id'],'continued')

    def test_partial_snapshot_omits_interior_unmatched_call_as_subset(self):
        self.write([self.user()]); admission=self.admit()['admission']
        call=self.row('assistant','call','u',[{'type':'tool_use','id':'t','name':'Read','input':{}}])
        later=self.row('assistant','later','call','Still waiting.')
        self.write([self.user(),call,later])
        result=self.observe(admission,stop_receipt=self.receipt('Still waiting.'),
                            boundary_receipt=self.boundary(kind='session_end'),allow_partial=True)
        self.assertEqual(result['status'],'observed',result)
        self.assertEqual(result['coverage']['public_evidence']['mode'],'selected_subset')
        self.assertEqual(result['coverage']['snapshot']['unmatched_calls'],1)
        self.assertEqual([v['kind'] for v in result['evidence']],['user_statement','assistant_assertion'])
        import recording_contract
        prompt, context=recording_contract.prepare(result)
        self.assertIsNotNone(prompt,context)
        self.assertEqual(json.loads(context.coverage_json)['public_evidence']['mode'],'selected_subset')

    def test_boundary_cannot_borrow_an_arbitrary_next_user_record(self):
        self.write([self.user()]);admission=self.admit()['admission']
        self.write([self.user(),self.assistant()]);self.seal()
        result=self.observe(admission,stop_receipt=self.receipt(),boundary_receipt=self.boundary('Unrelated input.'))
        self.assertEqual(result['reason'],'source_boundary_prompt_mismatch')

    def test_invalid_admissions_unknown_legacy_and_receipts_rejected(self):
        a=self.admit()['admission']; value=asdict(a)
        for bad in ({**value,'schema':'mneme.codex-turn-admission.v1'}, {**value,'start_ordinal':False}, {**value,'extra':1}):
            with self.assertRaises(ValueError): observer.restore_admission(bad)
        for bad in ({**self.receipt(),'agent_id':'child'},{**self.receipt(),'prompt_id':'other'},
                    {**self.receipt(),'hook_event_name':'StopFailure'}):
            with self.assertRaises(ValueError): observer.validate_stop_receipt(bad,session_id='session',turn_id='prompt')

    def test_repeated_prompt_and_complete_turn_cannot_be_admitted(self):
        self.write([self.user(),self.user()])
        self.assertEqual(self.admit()['reason'],'ambiguous_source_prompt')
        self.write([self.user(),self.assistant()])
        self.assertEqual(self.admit()['reason'],'not_current_open_turn')

    def test_malformed_oversized_and_unflushed_lines_fail_bounded(self):
        a=self.admit()['admission']
        for raw in (b'{bad}\n',b'{"type":"user"}',b' '*(observer.Limits().scan_bytes+1)):
            self.path.write_bytes(raw)
            result=self.observe(a,stop_receipt=self.receipt(),boundary_receipt=self.boundary())
            self.assertEqual(result['status'],'deferred')
            self.assertLessEqual(result['work']['bytes_read'],observer.observation_byte_ceiling(a))

    def test_prefix_rewrite_replacement_and_duplicate_chain_identity_defer(self):
        history=self.row('user','history',None,'Previous.')
        self.write([history]); a=self.admit()['admission']
        history['message']['content']='Rewritten'
        self.write([history,self.user(),self.assistant()])
        self.assertEqual(self.observe(a)['reason'],'source_preceding_anchor_changed')
        self.path.unlink();self.write([self.user()])
        self.assertEqual(self.observe(a)['reason'],'source_replaced')

if __name__=='__main__': unittest.main()

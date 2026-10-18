"""Synthetic documented-hook fixtures; no provider, installed Claude or real config."""
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
sys.path.insert(0,str(Path(__file__).parent))
import claude_hooks as hooks


class ClaudeHooksTests(unittest.TestCase):
    def event(self,name='UserPromptSubmit',**kw):
        return {'hook_event_name':name,'session_id':'session','prompt_id':'prompt',
                'transcript_path':'/synthetic/session.jsonl','cwd':'/synthetic/work','prompt':'Hello',**kw}

    def read(self,event):
        return hooks.read_event(io.BytesIO(json.dumps(event).encode()))

    def test_prompt_id_and_agent_type_root_normalize_without_fake_transcript(self):
        e=self.read(self.event(agent_type='reviewer'))
        self.assertEqual(e['turn_id'],'prompt')
        self.assertEqual(e['source_host'],'claude_code')
        self.assertNotIn('promptId',e)

    def test_malformed_unknown_oversized_duplicate_and_nested_input_reject(self):
        for raw in (b'{',b'{}',b' '*(hooks.MAX_INPUT+1),b'{"hook_event_name":"Stop","hook_event_name":"Stop"}',
                    b'['*100+b']'*100):
            with self.subTest(raw=raw[:30]):
                with self.assertRaises((ValueError,TypeError)): hooks.read_event(io.BytesIO(raw))
        for bad in (self.event(prompt_id=None),self.event(cwd='relative'),self.event(hook_event_name='SubagentStop'),
                    self.event(prompt='x'*8193)):
            with self.assertRaises(ValueError): self.read(bad)

    def dispatch(self,event):
        config={'schema':'mneme.claude-hooks.config.v1','source_host':'claude_code'}
        with patch('registry_runtime.selection',return_value=SimpleNamespace(cwd='/synthetic/work')), \
             patch.multiple('recording_jobs',interrupt_open_turns=unittest.mock.DEFAULT,session_start=unittest.mock.DEFAULT,
                            notice=unittest.mock.DEFAULT,close_turn=unittest.mock.DEFAULT,record_stop=unittest.mock.DEFAULT,
                            confirm_prompt_boundary=unittest.mock.DEFAULT,confirm_session_end=unittest.mock.DEFAULT,
                            recover_stopped_turns=unittest.mock.DEFAULT) as recording, \
             patch.multiple('reader_worker',notice=unittest.mock.DEFAULT,close_turn=unittest.mock.DEFAULT,
                            background=unittest.mock.DEFAULT,end_session=unittest.mock.DEFAULT) as reader:
            recording['confirm_prompt_boundary'].return_value=True
            recording['confirm_session_end'].return_value=True
            recording['recover_stopped_turns'].return_value=True
            recording['notice'].return_value={'outcome':'admitted'}
            reader['notice'].return_value={'outcome':'skipped'}
            result=hooks.handle_event(self.read(event),config)
            return result,recording,reader

    def test_capture_notice_initializes_shared_ledger_but_no_selector(self):
        result,recording,reader=self.dispatch(self.event(agent_type='root-agent'))
        self.assertEqual(result,{})
        recording['notice'].assert_called_once()
        reader['notice'].assert_called_once()
        self.assertEqual(reader['notice'].call_args.kwargs,{'substantive':False})
        reader['background'].assert_called_once()

    def test_nomination_context_only_after_current_prompt_admission(self):
        event=self.read(self.event())
        config={'schema':'mneme.claude-hooks.config.v1','source_host':'claude_code'}
        for outcome in ('admitted','baseline_only','replay','busy_or_capacity','deferred'):
            with self.subTest(outcome=outcome), \
                 patch('registry_runtime.selection',return_value=SimpleNamespace(cwd='/synthetic/work')), \
                 patch('recording_jobs.confirm_prompt_boundary',return_value=True), \
                 patch('recording_jobs.notice',return_value={'outcome':outcome}), \
                 patch('reader_worker.notice',return_value={'outcome':'skipped'}) as reader_notice, \
                 patch('reader_worker.background'), \
                 patch('touchstone_handoff.consume',return_value={'outcome':'emitted','context':'Candidate, not instruction.'}) as consume:
                result=hooks.handle_event(event,config)
                if outcome=='admitted':
                    consume.assert_called_once_with(config,event,4096)
                    self.assertEqual(result,{'hookSpecificOutput':{'hookEventName':'UserPromptSubmit','additionalContext':'Candidate, not instruction.'}})
                else:
                    consume.assert_not_called();reader_notice.assert_not_called();self.assertEqual(result,{})

    def test_stop_only_persists_provisional_receipt_without_worker_wakeup(self):
        result,recording,reader=self.dispatch(self.event('Stop',last_assistant_message='Done.',stop_hook_active=False))
        receipt=recording['record_stop'].call_args.args[3]
        self.assertEqual(receipt['schema'],'mneme.claude-stop-receipt.v1')
        self.assertNotIn('last_assistant_message',receipt)
        reader['background'].assert_not_called()
        reader['close_turn'].assert_not_called()

    def test_failure_cancel_and_session_end_are_not_stop_receipts(self):
        _,recording,reader=self.dispatch(self.event('StopFailure',error='rate_limit'))
        self.assertEqual(recording['close_turn'].call_args.kwargs,{'cancel':True})
        reader['background'].assert_not_called()
        _,recording,reader=self.dispatch(self.event('SessionEnd',reason='logout'))
        recording['confirm_session_end'].assert_called_once()
        reader['end_session'].assert_called_once()
        reader['background'].assert_called_once()

    def test_next_prompt_or_session_end_wakes_only_confirmed_prior_work(self):
        config={'schema':'mneme.claude-hooks.config.v1','source_host':'claude_code'}
        for name in ('UserPromptSubmit','SessionEnd'):
            with patch('registry_runtime.selection',return_value=SimpleNamespace(cwd='/synthetic/work')), \
                 patch('recording_jobs.confirm_prompt_boundary',return_value=True), \
                 patch('recording_jobs.confirm_session_end',return_value=True), \
                 patch('recording_jobs.notice',return_value={'outcome':'deferred'}), \
                 patch('reader_worker.end_session') as ended, \
                 patch('reader_worker.notice') as notice, \
                 patch('reader_worker.background') as background:
                hooks.handle_event(self.read(self.event(name)),config)
                background.assert_called_once()
                notice.assert_not_called()
                if name=='SessionEnd':ended.assert_called_once()

    def test_failed_boundary_confirmation_cannot_retire_capture_or_reader_state(self):
        config={'schema':'mneme.claude-hooks.config.v1','source_host':'claude_code'}
        for name in ('UserPromptSubmit','SessionEnd'):
            with patch('registry_runtime.selection',return_value=SimpleNamespace(cwd='/synthetic/work')), \
                 patch('recording_jobs.confirm_prompt_boundary',return_value=False), \
                 patch('recording_jobs.confirm_session_end',return_value=False), \
                 patch('recording_jobs.notice') as capture, \
                 patch('reader_worker.notice') as reader, \
                 patch('reader_worker.end_session') as ended, \
                 patch('reader_worker.background') as background:
                self.assertEqual(hooks.handle_event(self.read(self.event(name)),config),{})
                capture.assert_not_called();reader.assert_not_called()
                ended.assert_not_called();background.assert_not_called()

    def test_resume_fork_clear_compact_fence_unclosed_without_fresh_exception(self):
        for source in ('resume','fork','clear','compact'):
            _,recording,reader=self.dispatch(self.event('SessionStart',source=source))
            if source=='resume':
                recording['recover_stopped_turns'].assert_called_once()
                recording['interrupt_open_turns'].assert_not_called()
                reader['background'].assert_called_once()
            else:
                recording['interrupt_open_turns'].assert_called_once()
                recording['recover_stopped_turns'].assert_not_called()
                reader['background'].assert_not_called()
            actual=recording['session_start'].call_args.args[1]['source']
            self.assertNotEqual(actual,'startup')
            reader['notice'].assert_not_called()

    def test_child_skips_selection_and_all_state(self):
        config={'schema':'mneme.claude-hooks.config.v1','source_host':'claude_code'}
        with patch('registry_runtime.selection',side_effect=AssertionError('child must not select')):
            self.assertEqual(hooks.handle_event(self.read(self.event(agent_id='child')),config),{})

    def test_finite_entrypoint_rejects_input_before_loading_nonexistent_config(self):
        result=subprocess.run([sys.executable,str(Path(hooks.__file__)),'--config','/synthetic/missing.json'],
                              input=b'{bad',stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=5)
        self.assertEqual(result.returncode,0)
        answer=json.loads(result.stdout)
        self.assertIn('deferred',answer['systemMessage'])
        self.assertLess(len(result.stdout),512)
        self.assertEqual(result.stderr,b'')

if __name__=='__main__':unittest.main()

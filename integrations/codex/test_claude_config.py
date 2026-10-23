"""Review-only synthetic adapter fixtures; never read ~/.claude or invoke providers."""
import json
import hashlib
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
sys.path.insert(0,str(Path(__file__).parent))
import claude_config as config


class ClaudeConfigTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name).resolve()
        self.exe=self.root/'synthetic-claude';self.exe.write_text('#!/bin/sh\nexit 0\n');self.exe.chmod(0o755)
        self.memory=self.root/'memory.toml'
        self.config_path=self.root/'capture.json'

    def prepare(self, **overrides):
        return config.prepare(memory_config=self.memory,native_cli=self.exe,state_dir=self.root/'state',
            reader_claude=self.exe,reader_model='claude-sonnet-4-20250514',librarian_effort='medium',
            transcripts_root=self.root,hooks_program=Path(config.__file__).with_name('claude_hooks.py'),
            hook_config_path=self.config_path,**overrides)

    def test_prepare_is_review_only_no_config_store_or_provider_contact(self):
        with patch('store_registry.native_select',side_effect=AssertionError('prepare must not select owners')):
            result=self.prepare()
        self.assertEqual(result['installation'],'not performed')
        self.assertFalse(self.config_path.exists())
        self.assertFalse(self.memory.exists())
        self.assertFalse((self.root/'state').exists())
        self.assertNotIn('reader_codex',result['hook_config'])
        self.assertEqual(set(result['claude_settings']['hooks']),{'SessionStart','UserPromptSubmit','Stop','StopFailure','SessionEnd','PreCompact'})
        for groups in result['claude_settings']['hooks'].values():
            handler=groups[0]['hooks'][0]
            self.assertEqual(handler['type'],'command')
            self.assertIn('claude_hooks.py',handler['command'])
            self.assertNotIn('async',handler)

    def test_legacy_unknown_cross_host_paths_hashes_and_model_alias_reject(self):
        value=self.prepare()['hook_config']
        for bad in ({**value,'schema':'mneme.codex-hooks.config.v13'},{**value,'unknown':True},
                    {**value,'assessor':'codex'},{**value,'reader_model':'sonnet'},
                    {**value,'reader_model':'claude-sonnet'}, {**value,'reader_model':'claude-not-a-version'},{**value,'native_cli_sha256':'bad'},
                    {**value,'native_cli':'mnemed','native_cli_sha256':'a'*64},
                    {**value,'reader_claude':'claude','reader_claude_sha256':'a'*64},
                    {**value,'state_dir':'relative'},{**value,'state_dir':str(self.root)+'/../state'}):
            with self.subTest(bad=bad):
                with self.assertRaises(ValueError):config.validate_static(bad)

    def test_defaults_are_logical_and_absolute_paths_are_unpinned(self):
        value=self.prepare()['hook_config']
        self.assertNotIn('native_cli_sha256',value)
        self.assertNotIn('reader_claude_sha256',value)
        for key in ('native_cli','reader_claude','native_cli_sha256','reader_claude_sha256'):
            value.pop(key,None)
        loaded=config.validate_static(value)
        self.assertEqual(loaded['native_cli'],'mnemed')
        self.assertEqual(loaded['reader_claude'],'claude')

    def test_prepare_preserves_absolute_symlink_selector_without_hash(self):
        link=self.root/'current-claude'
        link.symlink_to(self.exe)
        with patch.object(self,'exe',link):
            value=self.prepare()['hook_config']
        self.assertEqual(value['reader_claude'],str(link))
        self.assertEqual(value['native_cli'],str(link))
        self.assertNotIn('reader_claude_sha256',value)

    def test_prepare_keeps_explicit_python_symlink_and_dynamic_default(self):
        link=self.root/'current-python'
        link.symlink_to(sys.executable)
        explicit=self.prepare(python=str(link))
        command=explicit['claude_settings']['hooks']['Stop'][0]['hooks'][0]['command']
        self.assertIn(str(link),command)
        self.assertNotIn(str(Path(sys.executable).resolve()),command)
        default=self.prepare()
        command=default['claude_settings']['hooks']['Stop'][0]['hooks'][0]['command']
        self.assertIn('python3',command)
        self.assertNotIn(str(Path(sys.executable).resolve()),command)
        with self.assertRaises(ValueError):
            self.prepare(python=str(self.root/'missing-python'))

    def test_explicit_native_hash_retains_call_and_replay_witness(self):
        from store_registry import NativeSelection, NativeClient, native_binary_identity
        import registry_runtime
        snapshot={'schema':'mneme.config-selection.v1','digest':'a'*64,
            'cwd':str(self.root),'config':{'background':{'enabled':True}},'active':[
                {'name':'fixture','store':{'id':'01ARZ3NDEKTSV4RRFFQ69G5FAV',
                    'owner':{'url':'http://127.0.0.1:1','db':'project','path':None}},
                 'eligible':{key:True for key in ('read','write','core','context','recall','record','maintenance')}}]}
        selected=NativeSelection(snapshot,str(self.memory),str(self.exe))
        value=self.prepare()['hook_config']
        self.config_path.write_text(json.dumps(value))
        with patch('store_registry.native_select',return_value=selected):
            unpinned=config.load_config(self.config_path,cwd=str(self.root))
        self.assertIsNone(unpinned['frozen_selection']['binary_pin'])
        value['native_cli_sha256']=hashlib.sha256(self.exe.read_bytes()).hexdigest()
        self.config_path.write_text(json.dumps(value))
        with patch('store_registry.native_select',return_value=selected):
            loaded=config.load_config(self.config_path,cwd=str(self.root))
            frozen=registry_runtime.selection(loaded)
        self.assertEqual(frozen.binary_pin,native_binary_identity(self.exe))
        client=NativeClient(frozen.named('fixture'),operation='explicit')
        self.exe.write_text('#!/bin/sh\nexit 9\n')
        with patch('store_registry.native_bridge',side_effect=AssertionError('native called')), \
             patch('store_registry.native_select',side_effect=AssertionError('replay selected')):
            with self.assertRaisesRegex(ValueError,'binary changed'):
                client.call_tool('status',{})
            with self.assertRaisesRegex(ValueError,'binary changed'):
                frozen.revalidate()

    def test_explicit_native_pin_detects_changes_during_hash_and_selection(self):
        value=self.prepare()['hook_config']
        value['native_cli_sha256']=hashlib.sha256(self.exe.read_bytes()).hexdigest()
        self.config_path.write_text(json.dumps(value))
        actual_hash=config._hash
        def changed_hash(path):
            digest=actual_hash(path)
            self.exe.write_text(self.exe.read_text()+'# replaced\n')
            return digest
        with patch.object(config,'_hash',side_effect=changed_hash), \
             patch('registry_runtime.native_capture_selection',side_effect=AssertionError('selected')):
            with self.assertRaisesRegex(ValueError,'during verification'):
                config.load_config(self.config_path,cwd=str(self.root))
        value['native_cli_sha256']=actual_hash(self.exe)
        self.config_path.write_text(json.dumps(value))
        def changed_selection(c,*args,**kwargs):
            self.exe.write_text(self.exe.read_text()+'# replaced again\n')
            return {**c,'frozen_selection':{'binary_pin':None}}
        with patch('registry_runtime.native_capture_selection',side_effect=changed_selection):
            with self.assertRaisesRegex(ValueError,'during selection'):
                config.load_config(self.config_path,cwd=str(self.root))

    def test_load_binds_explicit_hashed_executables_and_shared_capture_selection(self):
        value=self.prepare()['hook_config']
        value['native_cli_sha256']=value['reader_claude_sha256']=hashlib.sha256(self.exe.read_bytes()).hexdigest()
        self.config_path.write_text(json.dumps(value))
        def selected(c,path,raw,**kw):
            self.assertEqual(kw,{'cwd':str(self.root),'capture_only':True})
            self.assertEqual(c['source_host'],'claude_code')
            self.assertNotIn('reader_codex',c)
            return {**c,'_config_path':path,'frozen_selection':{'binary_pin':None}}
        with patch('registry_runtime.native_capture_selection',side_effect=selected) as select, \
             patch('recording_jobs._config_digest',return_value='pin'):
            loaded=config.load_config(self.config_path,cwd=str(self.root))
        self.assertEqual(loaded['recall_mode'],'off');self.assertFalse(loaded['_background_maintenance'])
        self.assertEqual(loaded['_reader_config_pin'],'pin');select.assert_called_once()
        self.exe.write_text('#!/bin/sh\nexit 1\n')
        with patch('registry_runtime.native_capture_selection',side_effect=AssertionError('hash changed')):
            with self.assertRaises(ValueError):config.load_config(self.config_path,cwd=str(self.root))

if __name__=='__main__':unittest.main()

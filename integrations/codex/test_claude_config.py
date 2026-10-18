"""Review-only synthetic adapter fixtures; never read ~/.claude or invoke providers."""
import json
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

    def prepare(self):
        return config.prepare(memory_config=self.memory,native_cli=self.exe,state_dir=self.root/'state',
            reader_claude=self.exe,reader_model='claude-sonnet-4-20250514',librarian_effort='medium',
            transcripts_root=self.root,hooks_program=Path(config.__file__).with_name('claude_hooks.py'),
            hook_config_path=self.config_path,python=Path(sys.executable).resolve())

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
                    {**value,'state_dir':'relative'},{**value,'state_dir':str(self.root)+'/../state'}):
            with self.subTest(bad=bad):
                with self.assertRaises(ValueError):config.validate_static(bad)

    def test_load_binds_explicit_hashed_executables_and_shared_capture_selection(self):
        value=self.prepare()['hook_config'];self.config_path.write_text(json.dumps(value))
        def selected(c,path,raw,**kw):
            self.assertEqual(kw,{'cwd':str(self.root),'capture_only':True})
            self.assertEqual(c['source_host'],'claude_code')
            self.assertNotIn('reader_codex',c)
            return {**c,'_config_path':path}
        with patch('registry_runtime.native_capture_selection',side_effect=selected) as select, \
             patch('recording_jobs._config_digest',return_value='pin'):
            loaded=config.load_config(self.config_path,cwd=str(self.root))
        self.assertEqual(loaded['recall_mode'],'off');self.assertFalse(loaded['_background_maintenance'])
        self.assertEqual(loaded['_reader_config_pin'],'pin');select.assert_called_once()
        self.exe.write_text('#!/bin/sh\nexit 1\n')
        with patch('registry_runtime.native_capture_selection',side_effect=AssertionError('hash changed')):
            with self.assertRaises(ValueError):config.load_config(self.config_path,cwd=str(self.root))

if __name__=='__main__':unittest.main()

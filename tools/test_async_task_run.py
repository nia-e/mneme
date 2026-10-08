import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import async_task_cases as cases
import async_task_run as runner


class RunnerTests(unittest.TestCase):
    def test_actor_projection_has_no_host_gold_or_file_contents(self):
        for case in cases.load()['cases']:
            prompt = runner.actor_prompt(case)
            for forbidden in ('accepted_plans', 'required_memory', 'information_gap', 'family', 'split'):
                self.assertNotIn('"' + forbidden + '"', prompt)
            for value in case['scene']['files'].values():
                self.assertNotIn(value, prompt)
            self.assertLess(len(prompt.encode()), 4096)

    def test_cue_matches_utf8_worker_cap(self):
        value = 'é' * 4097
        self.assertEqual(runner.reader_cue(value), 'Current task: ' + 'é' * 2048)

    def test_reservation_exclusive_durable_and_capped(self):
        with tempfile.TemporaryDirectory() as d:
            ledger = runner.Ledger(d)
            try:
                for n in range(30):
                    ledger.reserve('actor', str(n), 'off')
                with self.assertRaises(ValueError): ledger.reserve('actor', 'extra', 'off')
                with self.assertRaises(ValueError): ledger.reserve('actor', '0', 'off')
                for n in range(10): ledger.reserve('reader_slot', str(n), 'async')
                with self.assertRaises(ValueError): ledger.reserve('reader_slot', 'extra', 'async')
                self.assertEqual(len(Path(d, 'calls.jsonl').read_text().splitlines()), 40)
                with self.assertRaises(FileExistsError): runner.Ledger(d)
            finally:
                ledger.close()

    def test_receipt_never_overwrites(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / 'receipt.json'
            runner.write_new(p, {'first': True})
            with self.assertRaises(FileExistsError): runner.write_new(p, {'bad': True})
            self.assertEqual(json.loads(p.read_text()), {'first': True})

    def test_cli_inert_without_explicit_run(self):
        for args, code in [([], 2), (['--bad'], 2), (['--help'], 0), (['--run', 'x'], 2)]:
            result = subprocess.run([sys.executable, runner.__file__, *args], capture_output=True)
            self.assertEqual(result.returncode, code)

    def test_fresh_manifest_rejects_changed_fixture_before_run(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            fixture = root / 'cases.json'
            fixture.write_text('{"synthetic":true}')
            receipt = root / 'manifest.json'

            def current_manifest(codex, binaries):
                return {'codex': str(codex), 'binaries': str(binaries),
                        'files': {str(fixture): runner.sha(fixture)}}

            expected = current_manifest(root / 'codex', root / 'bin')
            runner.write_new(receipt, expected)
            with patch.object(runner, 'manifest', side_effect=current_manifest):
                self.assertEqual(runner.check_manifest(receipt), expected)
                fixture.write_text('{"synthetic":false}')
                with self.assertRaisesRegex(ValueError, 'manifest no longer matches'):
                    runner.check_manifest(receipt)


if __name__ == '__main__':
    unittest.main()

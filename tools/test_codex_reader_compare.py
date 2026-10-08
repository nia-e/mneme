"""Provider-free checks for the paired selector comparison."""
from __future__ import annotations

import copy
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import codex_reader_compare as compare


class ReaderComparisonTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.document = json.loads(compare.CASES.read_text())

    def test_inventory_rejects_conflicting_labels_and_truncated_input(self):
        self.assertEqual(len(compare.inventory(self.document)), 12)
        bad = copy.deepcopy(self.document)
        case = bad['cases'][0]
        case['host']['drop'].append(case['host']['keep'][0])
        with self.assertRaisesRegex(ValueError, 'gold partition'):
            compare.inventory(bad)
        bad = copy.deepcopy(self.document)
        bad['cases'][0]['dialogue'][0]['text'] = 'too long ' * 1000
        with self.assertRaisesRegex(ValueError, 'truncate'):
            compare.inventory(bad)

    def run_fake(self, root, *, live=False, fail_at=None):
        home = root / 'fake-home'
        binary = home / '.local/bin/codex'
        binary.parent.mkdir(parents=True)
        binary.write_text('not an executable; provider is mocked')
        auth = home / '.codex/auth.json'
        auth.parent.mkdir()
        auth.write_text('{}')
        output = root / 'results'
        calls = []

        def select(dialogue, cards, ledger, **kwargs):
            calls.append((copy.deepcopy(dialogue), copy.deepcopy(cards), kwargs))
            return {'selected_ids': [], 'reason': 'timeout' if len(calls) == fail_at else 'abstained',
                    'provider_attempt': True, 'cache_hit': False,
                    'usage': {'input_tokens': 20, 'cached_input_tokens': 10,
                              'uncached_input_tokens': 10, 'output_tokens': 2},
                    'elapsed_ms': 5}

        argv = ['compare', '--output', str(output)] + (['--run'] if live else [])
        with patch.object(Path, 'home', return_value=home), \
             patch.dict(os.environ, {'CODEX_HOME': str(auth.parent)}), \
             patch.object(sys, 'argv', argv), \
             patch.object(compare.reader, 'select', side_effect=select), \
             patch('builtins.print'):
            compare.main()
            with self.assertRaises(FileExistsError):
                compare.main()
        return json.loads((output / 'results.json').read_text()), calls

    def test_default_is_provider_free_and_refuses_overwrite(self):
        with tempfile.TemporaryDirectory() as name:
            result, calls = self.run_fake(Path(name))
        self.assertEqual(result['status'], 'prepared')
        self.assertEqual(calls, [])
        self.assertTrue(result['files'])

    def test_pairs_same_inputs_alternating_order_no_gold_in_payload(self):
        with tempfile.TemporaryDirectory() as name:
            result, calls = self.run_fake(Path(name), live=True)
        self.assertEqual((result['status'], result['attempts'], len(calls)), ('completed', 24, 24))
        for index in range(12):
            a, b = calls[index * 2:index * 2 + 2]
            self.assertEqual(a[:2], b[:2])
            self.assertEqual((a[2]['model'], b[2]['model']),
                             compare.MODELS if index % 2 == 0 else compare.MODELS[::-1])
            self.assertTrue(all(set(c) == {'id', 'summary', 'source', 'fingerprint'} for c in a[1]))
            self.assertEqual(a[2]['effort'], 'low')
            self.assertEqual(a[2]['workdir'], b[2]['workdir'])
            self.assertEqual(a[2]['home'], b[2]['home'])
        for summary in result['summary'].values():
            self.assertEqual(summary['valid'], 12)
            self.assertEqual(summary['correct_abstention'], 4)
            self.assertEqual(summary['usage']['input_tokens'], 240)

    def test_failed_call_stops_without_retry_or_counting_failure_as_bad_selection(self):
        with tempfile.TemporaryDirectory() as name:
            result, calls = self.run_fake(Path(name), live=True, fail_at=3)
        self.assertEqual((result['status'], len(calls)), ('stopped', 3))
        self.assertIsNone(result['results'][-1]['grade'])
        self.assertEqual(sum(s['valid'] for s in result['summary'].values()), 2)


if __name__ == '__main__':
    unittest.main()

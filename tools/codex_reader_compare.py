#!/usr/bin/env python3
"""Small paired reader comparison. Preparation is provider-free by default."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import sys
import tempfile

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'tools'))
import codex_reader as reader
from codex_reader_probe import retention

MODELS = ('gpt-5.6-sol', 'gpt-6-sol')
CASES = ROOT / 'tools/testdata/codex-reader-models-v1/cases.json'
OUTPUT = ROOT / 'target/codex-reader-models-v1'


def need(ok, message):
    if not ok:
        raise ValueError(message)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def inventory(document):
    need(document.get('version') == 1 and len(document.get('cases', [])) == 12,
         'expected twelve fresh cases')
    cases = document['cases']
    need(len({c['id'] for c in cases}) == 12, 'duplicate case IDs')
    need({k: sum(c['kind'] == k for c in cases) for k in ('useful', 'abstain', 'hard')}
         == {'useful': 4, 'abstain': 4, 'hard': 4}, 'unbalanced case kinds')
    for case in cases:
        cards, dialogue, host = case['cards'], case['dialogue'], case['host']
        ids = {c['id'] for c in cards}
        need(2 <= len(cards) <= 5 and len(ids) == len(cards), 'invalid card count/IDs')
        need(reader._window(dialogue) == dialogue, 'dialogue would truncate')
        need(reader._substantive(dialogue[-1]['text']), 'case would bypass reader')
        projected = reader._cards(cards)
        need(projected is not None, 'invalid reader card projection')
        for card in cards:
            need(card.get('source') and card.get('fingerprint'), 'missing source identity')
        groups = [set(host[key]) for key in ('keep', 'drop', 'allow')]
        need(not any(groups[i] & groups[j] for i in range(3) for j in range(i + 1, 3))
             and set.union(*groups) == ids and len(groups[0]) <= 2, 'invalid gold partition')
        if case['kind'] == 'abstain':
            need(not groups[0] and not groups[2], 'abstention must genuinely require none')
        prompt = reader.PROMPT_PREFIX + reader._encode({'dialogue': dialogue, 'cards': projected}).decode()
        need(len(prompt.encode()) <= reader.MAX_PROMPT_BYTES, 'prompt too large')
    return cases


def summarize(rows):
    result = {}
    for model in MODELS:
        rr = [r for r in rows if r['model'] == model]
        valid = [r for r in rr if r['result']['reason'] in ('selected', 'abstained')]
        usage = [r['result'].get('usage') for r in rr if r['result']['provider_attempt']]
        latencies = [r['result']['elapsed_ms'] for r in valid]
        keys = (*reader.USAGE_KEYS, 'uncached_input_tokens')
        result[model] = {
            'cases': len(rr), 'valid': len(valid),
            'acceptable': sum(r['grade']['exact_definite'] for r in valid),
            'missed_keep': sum(len(r['grade']['missed_keep']) for r in valid),
            'wrong_keep': sum(len(r['grade']['wrong_keep']) for r in valid),
            'correct_abstention': sum(r['kind'] == 'abstain' and not r['result']['selected_ids'] for r in valid),
            'by_kind': {k: {'cases': sum(r['kind'] == k for r in valid),
                            'acceptable': sum(r['kind'] == k and r['grade']['exact_definite'] for r in valid)}
                        for k in ('useful', 'abstain', 'hard')},
            'usage': {k: sum((u or {}).get(k) or 0 for u in usage) for k in keys},
            'missing_usage': {k: sum(u is None or u.get(k) is None for u in usage) for k in keys},
            'latency_ms_median': statistics.median(latencies) if latencies else None,
        }
    return result


def write(path, value):
    temp = path.with_suffix('.tmp')
    temp.write_text(json.dumps(value, indent=2, ensure_ascii=False) + '\n')
    temp.replace(path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run', action='store_true')
    parser.add_argument('--output', type=Path, default=OUTPUT)
    args = parser.parse_args()
    cases = inventory(json.loads(CASES.read_text()))
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    codex = (Path.home() / '.local/bin/codex').resolve()
    paths = [CASES, Path(__file__).resolve(), ROOT / 'tools/codex_reader.py',
             ROOT / 'tools/codex_reader_probe.py', ROOT / 'tools/test_codex_reader_compare.py',
             ROOT / 'tools/testdata/README.md', codex]
    receipt = {'schema': 'mneme.reader-model-comparison.v1', 'status': 'prepared',
               'models': MODELS, 'effort': 'low', 'max_attempts': 24,
               'files': {str(p): digest(p) for p in paths}, 'results': []}
    result_path = out / 'results.json'
    write(result_path, receipt)
    if not args.run:
        print(json.dumps({'status': 'prepared', 'output': str(result_path)}))
        return
    auth = Path(os.environ.get('CODEX_HOME', str(Path.home() / '.codex'))) / 'auth.json'
    need(codex.is_file() and auth.is_file(), 'missing Codex executable/auth')
    attempts = 0
    with tempfile.TemporaryDirectory(prefix='mneme-reader-models-') as tmp:
        base = Path(tmp).resolve()
        home, cwd = base / 'home', base / 'work'
        home.mkdir(mode=0o700)
        cwd.mkdir()
        (home / 'auth.json').symlink_to(auth)
        for index, case in enumerate(cases):
            cards = reader._cards(case['cards'])
            for model in (MODELS if index % 2 == 0 else MODELS[::-1]):
                need(attempts < 24, 'comparison attempt cap')
                result = reader.select(case['dialogue'], cards, out / 'ledger.json',
                    live=True, codex=codex, home=home, workdir=cwd, env=os.environ.copy(),
                    model=model, effort='low', scope=f"model-compare:{case['id']}:{model}")
                attempts += int(result['provider_attempt'])
                valid = result['reason'] in ('selected', 'abstained')
                row = {'case': case['id'], 'domain': case['domain'], 'kind': case['kind'],
                       'difficulty': case['difficulty'], 'model': model, 'result': result,
                       'grade': retention(result['selected_ids'], [c['id'] for c in case['cards']],
                                          case['host']) if valid else None}
                receipt['results'].append(row)
                receipt['status'] = 'running' if valid else 'stopped'
                receipt['attempts'] = attempts
                receipt['summary'] = summarize(receipt['results'])
                write(result_path, receipt)
                if not valid:
                    print(json.dumps({'status': 'stopped', 'reason': result['reason'], 'output': str(result_path)}))
                    return
    receipt['status'] = 'completed'
    write(result_path, receipt)
    print(json.dumps(receipt['summary'], indent=2))


if __name__ == '__main__':
    main()

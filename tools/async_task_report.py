#!/usr/bin/env python3
"""Offline, deterministic summaries of a completed or partial async task run."""
import argparse
from collections import Counter
import json
from pathlib import Path
import statistics


def summarize(rows, cases):
    by_case = {c['id']: c for c in cases}
    details = []
    for row in rows:
        case = by_case[row['case']]
        actor = row.get('actor', {})
        decision = actor.get('first_submission') or {}
        at = decision.get('received_monotonic_ns')
        observations = row.get('observations', [])
        inverse = {value: key for key, value in row.get('identifiers', {}).items()}
        def names(ids):
            return [inverse.get(i, 'unknown:' + str(i)) for i in ids]
        native = row.get('native', {}).get('pool', {})
        if row['arm'] == 'async':
            pools = [r for r in observations if r.get('event') == 'native_pool']
            native = pools[0] if pools else {}
        pool_ids = [c['id'] for c in native.get('cards', [])]
        selectors = [r for r in observations if r.get('event') == 'reader_select_complete']
        selected_ids = (row.get('native', {}).get('selected_ids', []) if row['arm'] == 'native'
                        else [i for r in selectors for i in r.get('selected_ids', [])])
        flushes = [r for r in observations if r.get('event') == 'hook_stdout_flushed'
                   and r.get('hook_event_name') == 'PostToolUse' and r.get('cards')]
        emitted_ids = [c['id'] for r in flushes for c in r['cards']]
        timely_ids = [c['id'] for r in flushes if at is not None
                      and r['completed_monotonic_ns'] < at for c in r['cards']]
        ready = [r for r in observations if r.get('event') == 'state_transaction'
                 and r.get('success') is True and (r.get('after') or {}).get('ready')]
        required = set(case['host']['required_memory'])
        selected = set(names(selected_ids))
        timely = set(names(timely_ids)) if row['arm'] == 'async' else selected
        reader_started = [r for r in observations if r.get('event') == 'reader_select_enter']
        item = {'case': row['case'], 'split': row['split'], 'arm': row['arm'],
                'actor_status': actor.get('status', 'not_started'),
                'grade': decision.get('grade'), 'actions': decision.get('answer', {}).get('actions')
                    if isinstance(decision.get('answer'), dict) else None,
                'decision_ms': round((at-actor['started_monotonic_ns'])/1e6, 2) if at else None,
                'native_outcome': native.get('outcome', 'not_observed'),
                'pool': names(pool_ids), 'selected': names(selected_ids),
                'required': sorted(required), 'required_available': sorted(required & set(names(pool_ids))),
                'required_selected': sorted(required & selected), 'required_timely': sorted(required & timely),
                'irrelevant_selected': sorted(set(case['host']['irrelevant_memory']) & selected),
                'emitted': names(emitted_ids), 'emitted_before_decision': names(timely_ids),
                'emission_bytes': [r['context_bytes'] for r in flushes],
                'ready_before_decision': bool(at and any(r['end_monotonic_ns'] < at for r in ready)),
                'tool_boundary_before_submission': actor.get('tool_boundary_before_submission'),
                'reader_select_entries': len(reader_started),
                'reader_attempt_reported': any(r.get('provider_attempt') is True for r in selectors),
                'reader_result': [r.get('reason') for r in selectors],
                'reader_usage': [r.get('usage') for r in selectors if r.get('usage')],
                'reader_usage_unknown': bool(reader_started) and not any(r.get('usage') for r in selectors),
                'actor_usage': actor.get('usage', {}), 'errors': actor.get('errors', []),
                'cleanup_ok': row.get('cleanup', {}).get('ok'),
                'model_visibility': 'unknown', 'causal_memory_use': 'not_identified'}
        offpool = row.get('graph_off_diagnostic', {})
        if offpool:
            item['graph_off_pool'] = names([c['id'] for c in offpool.get('cards', [])])
            item['graph_changes_pool'] = item['pool'] != item['graph_off_pool']
            item['graph_comparison_qualified'] = (native.get('outcome') == 'ok'
                                                  and offpool.get('outcome') == 'ok')
        details.append(item)
    totals = {}
    for arm in ('off', 'native', 'async'):
        subset = [r for r in details if r['arm'] == arm]
        resolutions = Counter((r['grade'] or {}).get('resolution', 'unobserved') for r in subset)
        usage = Counter()
        for r in subset:
            usage.update(r['actor_usage'].get('totals') or {})
        reader_usage = Counter()
        for r in subset:
            for record in r['reader_usage']:
                reader_usage.update(record)
        timing = [r['decision_ms'] for r in subset if r['decision_ms'] is not None]
        totals[arm] = {'cells': len(subset), 'success': sum(bool((r['grade'] or {}).get('success')) for r in subset),
                       'resolutions': dict(resolutions), 'actor_usage': dict(usage),
                       'actor_usage_unknown_cells': sum(r['actor_usage'].get('status') != 'complete' for r in subset),
                       'reader_usage': dict(reader_usage),
                       'reader_usage_unknown_cells': sum(r['reader_usage_unknown'] for r in subset),
                       'reader_attempts_reported': sum(r['reader_attempt_reported'] for r in subset),
                       'timely_emission_cells': sum(bool(r['emitted_before_decision']) for r in subset),
                       'emission_cells': sum(bool(r['emitted']) for r in subset),
                       'median_decision_ms': statistics.median(timing) if timing else None,
                       'all_cleanup_ok': all(r['cleanup_ok'] is True for r in subset)}
    return {'schema': 'mneme.async-task-summary.v1', 'arms': totals, 'cases': details,
            'boundary': 'Host receipt/flush observations, not visibility or causal use. No learning or scale test.'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('run', type=Path)
    args = parser.parse_args()
    import async_task_cases
    paths = sorted(args.run.glob('a??-*.json'))
    print(json.dumps(summarize([json.loads(p.read_text()) for p in paths],
                              async_task_cases.load()['cases']), indent=2))


if __name__ == '__main__':
    main()

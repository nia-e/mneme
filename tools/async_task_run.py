#!/usr/bin/env python3
"""Finite, explicitly invoked off/native/async comparison; never resumes or retries.

--prepare freezes code/artifacts without retrieval or providers. --run requires
that exact manifest and a new output directory. Partial runs remain partial.
"""
from __future__ import annotations
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_BIN = ROOT / 'target/reflexive-shadow-v1/default/bin'
CODEX = Path.home() / '.local/bin/codex'
ARMS = ('off', 'native', 'async')
MAX_ACTORS = 30
MAX_READERS = 10


def sha(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as f:
        for block in iter(lambda: f.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def write_new(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open('x') as f:
        json.dump(value, f, indent=2, ensure_ascii=False)
        f.write('\n')
        f.flush()
        os.fsync(f.fileno())


def manifest(codex=CODEX, binaries=DEFAULT_BIN):
    # Fresh manifests pin the current fixtures and scorer source below. A
    # machine-local historical scorer receipt is neither needed nor portable.
    paths = [ROOT / 'tools' / (name + '.py') for name in
             ('async_task_cases', 'async_task_host', 'async_task_fixture', 'async_task_observer',
              'async_task_run', 'async_task_report', 'test_async_task_cases', 'test_async_task_host',
              'test_async_task_fixture', 'test_async_task_observer', 'test_async_task_run',
              'reflexive_shadow_smoke', 'memory_library_smoke')]
    cache = Path.home() / '.local/share/mneme/.fastembed_cache/models--Xenova--bge-base-en-v1.5'
    revision = (cache / 'refs/main').read_text().strip()
    paths += [cache / 'refs/main'] + sorted(p for p in (cache / 'snapshots' / revision).rglob('*')
                                          if p.is_file())
    paths += sorted((ROOT / 'integrations/codex').glob('*.py'))
    paths += [ROOT / 'tools/testdata/async-task-v1/cases.json',
              ROOT / 'tools/testdata/README.md',
              ROOT / 'target/reflexive-shadow-v1/build-inventory.json',
              Path(codex).resolve(), Path(binaries) / 'mnemed', Path(binaries) / 'mneme-mcp']
    return {'schema': 'mneme.async-task-run-manifest.v1',
            'files': {str(p.resolve()): sha(p) for p in paths},
            'codex': str(Path(codex).resolve()), 'binaries': str(Path(binaries).resolve()),
            'actor_model': 'gpt-5.6-sol', 'actor_effort': 'low',
            'reader_model': 'gpt-5.6-sol', 'reader_effort': 'low',
            'limits': {'actor_launches': MAX_ACTORS, 'reader_reservations': MAX_READERS},
            'embedding': 'fastembed:Xenova/bge-base-en-v1.5:onnx-fp32:mneme-adapter-v1',
            'embedding_revision': revision,
            'embedding_readiness': 'fresh service; catalog only; lazy embedder not query-warmed',
            'retries': 'No harness retries. CLI internal retry count not observable.',
            'visibility': 'unknown; hook emission is not model acknowledgment'}


def check_manifest(path):
    expected = json.loads(Path(path).read_text())
    actual = manifest(Path(expected['codex']), Path(expected['binaries']))
    if actual != expected:
        raise ValueError('run manifest no longer matches; do not run or quietly re-freeze')
    return expected


def actor_prompt(case):
    import async_task_cases
    scene = async_task_cases.actor_scene(case)
    return ('Choose a decision plan, not an implemented patch. Inspect the listed local files if useful, '
            'but stay within this working directory; do not search parent directories, repositories, '
            'or external services. Do not wait, poll, or add tools to obtain memory. '
            'Use only the listed action strings and at most max_actions. '
            'Submit one JSON object {"actions":[...],"rationale":"brief reason"}, without Markdown. '
            'Do not revise a submitted plan. This disposable task creates no durable memory. '
            + ('Do not call any tools. ' if scene['tool_policy'] == 'none' else '')
            + '\nTask:\n' + json.dumps(scene, ensure_ascii=False, separators=(',', ':')))


def reader_cue(prompt):
    return 'Current task: ' + prompt.encode()[:4096].decode('utf-8', 'ignore')


class Ledger:
    """Reserve before any launch; a crash consumes its slot. Exclusive fresh run only."""
    def __init__(self, directory):
        self.path = Path(directory) / 'calls.jsonl'
        self.stream = self.path.open('x')
        self.counts = {'actor': 0, 'reader_slot': 0}
        self.seen = set()

    def reserve(self, kind, case, arm):
        key = kind, case, arm
        cap = MAX_ACTORS if kind == 'actor' else MAX_READERS if kind == 'reader_slot' else 0
        if key in self.seen or self.counts.get(kind, cap) >= cap:
            raise ValueError('call budget or duplicate reservation')
        self.seen.add(key)
        self.counts[kind] += 1
        value = {'kind': kind, 'case': case, 'arm': arm, 'ordinal': self.counts[kind],
                 'reserved_monotonic_ns': time.monotonic_ns()}
        self.stream.write(json.dumps(value) + '\n')
        self.stream.flush()
        os.fsync(self.stream.fileno())
        return value

    def close(self):
        self.stream.close()


def read_observations(path):
    if not Path(path).exists():
        return []
    with Path(path).open('rb') as stream:
        fcntl.flock(stream, fcntl.LOCK_SH)
        raw = stream.read(1024 * 1024 + 1)
    if len(raw) > 1024 * 1024:
        raise ValueError('observer trace exceeded bound')
    return [json.loads(line) for line in raw.splitlines() if line.strip()]


def drain_reader(path, timeout=50):
    """Observe late work only AFTER actor exit; never buy a delivery boundary."""
    started = time.monotonic()
    start_ns = time.monotonic_ns()
    outcome = 'deadline'
    while time.monotonic() - started < timeout:
        rows = read_observations(path)
        if any(r.get('event') in ('reader_select_complete', 'reader_select_error',
                                 'reader_cap_refused') for r in rows):
            outcome = 'selection_finished'
            break
        pools = [r for r in rows if r.get('event') == 'native_pool']
        if pools and (pools[-1].get('outcome') != 'ok' or not pools[-1].get('cards')):
            outcome = 'no_reader_needed'
            break
        if time.monotonic() - started >= 5 and not any(
                r.get('event') == 'reader_select_enter' for r in rows):
            outcome = 'no_selection_observed'
            break
        time.sleep(.05)
    return {'outcome': outcome, 'start_monotonic_ns': start_ns,
            'end_monotonic_ns': time.monotonic_ns(),
            'elapsed_ms': round((time.monotonic()-started)*1000),
            'actor_already_exited': True, 'not_a_delivery_opportunity': True}


def run(manifest_path, output, *, fixture_factory=None, actor_runner=None):
    import async_task_cases
    from async_task_fixture import fixture
    from async_task_host import run_actor
    from async_task_observer import install_observer
    frozen = check_manifest(manifest_path)
    fixture_factory = fixture_factory or fixture
    actor_runner = actor_runner or run_actor
    output = Path(output)
    output.mkdir(parents=True, exist_ok=False)
    write_new(output / 'manifest.json', frozen)
    document = async_task_cases.load()
    cards = {c['key']: c for c in document['cards']}
    cases = sorted(document['cases'], key=lambda c: (c['split'] == 'holdout', c['id']))
    ledger = Ledger(output)
    rows = []
    try:
        with tempfile.TemporaryDirectory(prefix='mneme-async-task-') as temporary:
            base = Path(temporary)
            for index, case in enumerate(cases):
                rotation = index % len(ARMS)
                order = ARMS[rotation:] + ARMS[:rotation]
                template = base / ('template-' + case['id'])
                for arm in order:
                    row = {'case': case['id'], 'split': case['split'], 'arm': arm,
                           'memory_visibility': 'unknown', 'memory_use': 'not_causally_identified'}
                    trace = base / (case['id'] + '-' + arm + '.jsonl')
                    active_fixture = None
                    print(json.dumps({'starting': case['id'], 'arm': arm}), flush=True)
                    try:
                        with fixture_factory(case, cards, base / (case['id'] + '-' + arm),
                             mnemed=Path(frozen['binaries']) / 'mnemed',
                             mcp=Path(frozen['binaries']) / 'mneme-mcp',
                             codex=Path(frozen['codex']), auth=Path.home() / '.codex/auth.json',
                             seed_template=template) as fx:
                            active_fixture = fx
                            row['fixture'] = fx.manifest
                            row['identifiers'] = fx.identifiers
                            prompt = actor_prompt(case)
                            if arm == 'async':
                                row['reader_slot'] = ledger.reserve('reader_slot', case['id'], arm)
                                row['instrumentation'] = install_observer(fx.home_hooks, fx.prefix, trace)
                            else:
                                fx.home_hooks.unlink()
                                if arm == 'native':
                                    native = fx.native_baseline(reader_cue(prompt))
                                    row['native'] = native
                                    if native.get('context'):
                                        prompt += '\n' + native['context']
                            row['actor_reservation'] = ledger.reserve('actor', case['id'], arm)
                            row['actor'] = actor_runner(codex=Path(frozen['codex']),
                                project=fx.project, home=fx.home, env=fx.env, prompt=prompt,
                                case=case, timeout=120)
                            if arm == 'async':
                                row['post_actor_drain'] = drain_reader(trace)
                            # No diagnostic query before first actual task read. Diagnostic
                            # may warm this arm after its decision, never the next fresh service.
                            if arm == 'native':
                                row['graph_off_diagnostic'] = fx.collect(reader_cue(actor_prompt(case)), depth=0)
                    except Exception as error:
                        # Fixed classification/hash, never an arbitrary auth/log payload.
                        row['harness_error'] = {'type': type(error).__name__,
                            'sha256': hashlib.sha256(str(error).encode()).hexdigest()}
                    finally:
                        if active_fixture is not None:
                            row['cleanup'] = active_fixture.cleanup
                            row['kv_before'] = active_fixture.before
                            row['kv_after'] = active_fixture.after
                        row['observations'] = read_observations(trace)
                    rows.append(row)
                    write_new(output / (case['id'] + '-' + arm + '.json'), row)
                    decision = row.get('actor', {}).get('first_submission') or {}
                    print(json.dumps({'finished': case['id'], 'arm': arm,
                         'status': row.get('actor', {}).get('status', 'harness_error'),
                         'grade': decision.get('grade'), 'calls': ledger.counts}), flush=True)
                    # A cleanup failure must not leave sessions/services accumulating.
                    if active_fixture is None:
                        raise RuntimeError('fixture setup failed; cleanup unavailable; stop experiment')
                    if active_fixture is not None and active_fixture.cleanup.get('ok') is not True:
                        raise RuntimeError('fixture cleanup failed; stop finite experiment')
                    if row.get('actor', {}).get('cleanup_complete') is False:
                        raise RuntimeError('actor cleanup failed; stop finite experiment')
        result = {'schema': 'mneme.async-task-run.v1', 'status': 'finished',
                  'counts': ledger.counts, 'rows': rows}
        write_new(output / 'results.json', result)
        return result
    finally:
        ledger.close()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--prepare', type=Path, metavar='NEW_MANIFEST')
    mode.add_argument('--run', type=Path, metavar='FROZEN_MANIFEST')
    parser.add_argument('--output', type=Path)
    args = parser.parse_args(argv)
    if args.prepare:
        if args.output:
            parser.error('--output is only for --run')
        write_new(args.prepare, manifest())
        print('Prepared without retrieval or provider calls: ' + str(args.prepare))
    else:
        if not args.output:
            parser.error('--run requires --output NEW_DIRECTORY')
        run(args.run, args.output)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())

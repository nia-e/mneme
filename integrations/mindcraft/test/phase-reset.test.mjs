import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtemp, mkdir, readFile, writeFile, readdir, symlink, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { resetAgentMemory } from '../phase-reset.mjs';

async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), 'mneme-mindcraft-reset-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  const botsRoot = join(root, 'bots');
  await mkdir(join(botsRoot, 'Alice'), { recursive: true });
  const path = join(botsRoot, 'Alice', 'memory.json');
  const original = JSON.stringify({ memory: 'Remember the house promise.', turns: [{ role: 'user', content: 'Secret predecessor transcript' }], self_prompting_state: 1, self_prompt: 'Keep digging.', taskStart: 42, last_sender: 'Bob', future_field: 'never carry unknown state' });
  await writeFile(path, original);
  return { root, botsRoot, path, original };
}

for (const condition of ['mneme', 'native', 'empty']) {
  test(`${condition}: reset drops transcript and task state, preserving only assigned memory`, async t => {
    const f = await fixture(t);
    const receiptPath = join(f.root, 'reset.json');
    const receipt = await resetAgentMemory({ botsRoot: f.botsRoot, agent: 'Alice', condition, receiptPath });
    const after = JSON.parse(await readFile(f.path, 'utf8'));
    assert.deepEqual(after, { memory: condition === 'native' ? 'Remember the house promise.' : '', turns: [], self_prompting_state: 0, self_prompt: null, taskStart: null, last_sender: null });
    assert.equal(await readFile(receipt.backup, 'utf8'), f.original);
    assert.equal(receipt.status, 'reset');
    assert.equal(JSON.parse(await readFile(receiptPath, 'utf8')).status, 'reset');
    assert.notEqual(receipt.before_sha256, receipt.after_sha256);
    await assert.rejects(resetAgentMemory({ botsRoot: f.botsRoot, agent: 'Alice', condition, receiptPath }), { code: 'EEXIST' });
    assert.deepEqual(JSON.parse(await readFile(f.path, 'utf8')), after);
  });
}

test('newcomer has no native summary to inherit', async t => {
  const f = await fixture(t);
  const receipt = await resetAgentMemory({ botsRoot: f.botsRoot, agent: 'Newcomer', condition: 'native', receiptPath: join(f.root, 'newcomer.json') });
  assert.equal(receipt.backup, null);
  assert.equal(receipt.before_sha256, null);
  assert.equal(JSON.parse(await readFile(receipt.history, 'utf8')).memory, '');
  assert.equal(await readFile(f.path, 'utf8'), f.original);
});

test('wrong schema, agent path and condition refuse without changing history', async t => {
  const f = await fixture(t);
  for (const extra of [{ agent: '../Bob' }, { condition: 'notes' }, { receiptPath: f.path }]) {
    await assert.rejects(resetAgentMemory({ botsRoot: f.botsRoot, agent: 'Alice', condition: 'mneme', receiptPath: join(f.root, 'refused.json'), ...extra }));
    assert.equal(await readFile(f.path, 'utf8'), f.original);
  }
  await writeFile(f.path, '{"some":"other application"}');
  await assert.rejects(resetAgentMemory({ botsRoot: f.botsRoot, agent: 'Alice', condition: 'mneme', receiptPath: join(f.root, 'bad-schema.json') }), /not a Mindcraft history/);
  assert.equal(await readFile(f.path, 'utf8'), '{"some":"other application"}');
  assert.deepEqual(await readdir(join(f.botsRoot, 'Alice')), ['memory.json']);
});

test('symlinked history and agent directories refuse without touching their targets', async t => {
  const f = await fixture(t);
  const target = join(f.root, 'other.json');
  await writeFile(target, f.original);
  await rm(f.path);
  await symlink(target, f.path);
  const args = { botsRoot: f.botsRoot, agent: 'Alice', condition: 'empty', receiptPath: join(f.root, 'refused.json') };
  await assert.rejects(resetAgentMemory(args), /single-link regular/);
  await symlink(join(f.botsRoot, 'Alice'), join(f.botsRoot, 'Bob'));
  await assert.rejects(resetAgentMemory({ ...args, agent: 'Bob' }), /real directory/);
  assert.equal(await readFile(target, 'utf8'), f.original);
});

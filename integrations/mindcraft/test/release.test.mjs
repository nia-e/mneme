import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { parseReleaseArgs, releaseDatabase } from '../release.mjs';

const DB_ID = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const ACK = { db: 'trial', db_id: DB_ID, state: 'maintenance', in_flight: 0,
  backend_jobs: 0, authority_rotated: true, resolved_path: '/tmp/disposable.db' };

async function fixture(t, mode) {
  const dir = await mkdtemp(join(tmpdir(), 'mindcraft-release-'));
  const requests = [];
  const server = createServer(async (req, res) => {
    let text = '';
    for await (const chunk of req) text += chunk;
    const body = text ? JSON.parse(text) : null;
    requests.push(body?.method === 'tools/call' ? body.params : body?.method ?? req.method);
    const send = (result) => { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify({ jsonrpc: '2.0', id: body.id, result })); };
    if (body?.method === 'initialize') { res.setHeader('mcp-session-id', 'operator-session'); send({ protocolVersion: '2025-11-25' }); return; }
    assert.equal(req.headers['mcp-session-id'], 'operator-session');
    if (req.method === 'DELETE') { res.writeHead(204); res.end(); return; }
    if (body?.method === 'notifications/initialized') { res.writeHead(202); res.end(); return; }
    assert.deepEqual(body.params, { name: 'database_control', arguments: { db: 'trial', action: 'release' } });
    if (mode === 'ambiguous') { res.destroy(); return; }
    if (mode === 'denied') { send({ content: [{ type: 'text', text: 'operator capability required' }], isError: true }); return; }
    const ack = mode === 'wrong-state' ? { ...ACK, state: 'open' } : mode === 'wrong-db' ? { ...ACK, db: 'other' } : mode === 'wrong-id-type' ? { ...ACK, db_id: [DB_ID] } : ACK;
    send({ content: [{ type: 'text', text: JSON.stringify(ack) }], isError: false });
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(async () => { server.closeAllConnections(); await new Promise((resolve) => server.close(resolve)); await rm(dir, { recursive: true, force: true }); });
  return { requests, config: { url: `http://127.0.0.1:${server.address().port}/`, db: 'trial', trace: join(dir, 'operator.jsonl') } };
}

test('explicit release arguments have no default target or recovery switch', () => {
  const args = ['--url', 'http://127.0.0.1:1234/', '--db', 'trial', '--trace', '/tmp/new.jsonl'];
  assert.deepEqual(parseReleaseArgs(args), { url: args[1], db: 'trial', trace: args[5] });
  for (const invalid of [[], args.slice(0, 4), [...args, '--resume'], ['--db', 'a', '--db', 'b', '--trace', '/tmp/t'], ['--url', '', '--db', 'a', '--trace', '/tmp/t']]) assert.throws(() => parseReleaseArgs(invalid));
});

test('operator release uses exact existing action once, verifies maintenance and never resumes', async (t) => {
  const f = await fixture(t, 'success');
  assert.deepEqual(await releaseDatabase(f.config), ACK);
  assert.deepEqual(f.requests, ['initialize', 'notifications/initialized', { name: 'database_control', arguments: { db: 'trial', action: 'release' } }, 'DELETE']);
  const trace = await readFile(f.config.trace, 'utf8');
  assert(trace.includes('"state":"maintenance"'));
  assert(!trace.includes('operator-session'));
});

for (const mode of ['denied', 'ambiguous', 'wrong-state', 'wrong-db', 'wrong-id-type']) {
  test(`operator release ${mode} is an explicit failure without replay`, async (t) => {
    const f = await fixture(t, mode);
    await assert.rejects(releaseDatabase(f.config), (error) => error.code === (mode === 'denied' ? 'TOOL' : mode === 'ambiguous' ? 'TRANSPORT' : 'PROTOCOL') && error.ambiguousWrite === true);
    assert.equal(f.requests.filter((request) => typeof request === 'object').length, 1);
    assert.equal(f.requests.at(-1), 'DELETE');
    assert((await readFile(f.config.trace, 'utf8')).includes('"event":"error"'));
  });
}

test('session cleanup failure does not replace original ambiguous release outcome', async () => {
  const original = Object.assign(new Error('release outcome unknown'), { ambiguousWrite: true });
  await assert.rejects(releaseDatabase({}, { createClient: () => ({
    releaseDatabase: async () => { throw original; }, close: async () => { throw new Error('close failed'); },
  }) }), (error) => error === original && error.closeError === 'close failed');
});

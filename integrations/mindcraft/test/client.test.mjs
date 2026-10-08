import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm, stat } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { MnemeClient, LIMITS } from '../mcp-client.mjs';

const ID = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const ID2 = '01ARZ3NDEKTSV4RRFFQ69G5FAW';

async function fixture(t, custom = () => false) {
  const dir = await mkdtemp(join(tmpdir(), 'mindcraft-client-'));
  const requests = [];
  let serial = 0;
  const sessions = new Set();
  const server = createServer(async (req, res) => {
    let text = '';
    for await (const chunk of req) text += chunk;
    const body = text ? JSON.parse(text) : null;
    requests.push({ method: req.method, headers: req.headers, body });
    const send = (result) => { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify({ jsonrpc: '2.0', id: body?.id, result })); };
    if (custom({ req, res, body, send, sessions })) return;
    if (body?.method === 'initialize') {
      assert.equal(req.headers['mcp-session-id'], undefined);
      const session = `session-${++serial}`;
      sessions.add(session); res.setHeader('mcp-session-id', session);
      send({ protocolVersion: '2025-11-25', capabilities: { tools: {} } });
      return;
    }
    if (!sessions.has(req.headers['mcp-session-id'])) { res.writeHead(404); res.end('unknown or expired mcp-session-id'); return; }
    assert.equal(req.headers['mcp-protocol-version'], '2025-11-25');
    if (req.method === 'DELETE') { sessions.delete(req.headers['mcp-session-id']); res.writeHead(204); res.end(); return; }
    if (body.method === 'notifications/initialized') { res.writeHead(202); res.end(); return; }
    const args = body.params.arguments;
    const payload = body.params.name === 'ingest' ? { id: ID, db: args.db, db_id: 'fixture' } :
      body.params.name === 'supersede' ? { ok: true, db: args.db, db_id: 'fixture' } :
        { id: ID, summary: 'Retained old advice', body: 'Authored evidence', status: 'candidate', received: args };
    send({ content: [{ type: 'text', text: JSON.stringify(payload) }], isError: false });
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(async () => { server.closeAllConnections(); await new Promise((resolve) => server.close(resolve)); await rm(dir, { recursive: true, force: true }); });
  const config = { url: `http://127.0.0.1:${server.address().port}/`, db: 'trial', actor: 'scout', tracePath: join(dir, 'trace.jsonl') };
  return { dir, requests, sessions, config, make: (extra = {}) => new MnemeClient({ ...config, ...extra }) };
}

test('lazy finite lifecycle, explicit body policy, exact decoded results and trace costs', async (t) => {
  const f = await fixture(t);
  const client = f.make();
  await assert.rejects(stat(f.config.tracePath), { code: 'ENOENT' });
  assert.equal(f.requests.length, 0);
  const ingested = await client.remember('Tunnel floods', 'Saw water after opening gravel.');
  assert.equal(ingested.id, ID);
  const request = f.requests.find((r) => r.body?.params?.name === 'ingest').body.params.arguments;
  assert.deepEqual(request, {
    db: 'trial', summary: 'Tunnel floods', tags: ['mindcraft'],
    body: JSON.stringify({ kind: 'mindcraft-observation', observer: 'scout', evidence: 'Saw water after opening gravel.' }),
  }, 'only supported ingest fields; provenance and explicit scope are retained');
  assert.equal((await client.recall('tunnel')).status, 'candidate', 'does not hide old advice');
  assert.equal((await client.get(ID)).body, 'Authored evidence');
  assert.equal((await client.supersede(ID2, ID)).ok, true);
  await client.close(); await client.close();
  assert.deepEqual(f.requests.map((r) => r.body?.method ?? r.method), ['initialize', 'notifications/initialized', 'tools/call', 'tools/call', 'tools/call', 'tools/call', 'DELETE']);
  await assert.rejects(client.get(ID), { code: 'CLOSED' });
  const trace = (await readFile(f.config.tracePath, 'utf8')).trim().split('\n').map(JSON.parse);
  assert.deepEqual(trace.map((entry) => entry.seq), trace.map((_, i) => i + 1));
  let bytes = 0;
  for (const entry of trace.filter((entry) => entry.event === 'result')) {
    bytes += entry.requestBytes + entry.responseBytes;
    assert.equal(entry.totalBytes, bytes);
    assert(Number.isFinite(entry.durationMs) && entry.durationMs >= 0);
  }
  assert(trace.some((entry) => entry.payload?.id === ID));
  assert(!JSON.stringify(trace).includes('session-1'), 'session header is not written to traces');
});

test('configuration refuses remote endpoints, credentials, implicit db and lossy limits', async (t) => {
  const f = await fixture(t);
  for (const url of ['https://example.com/', 'http://localhost/', 'http://127.0.0.1/?secret=x', 'http://u:p@127.0.0.1/', 'file:///tmp/a', 'http://127.0.0.1/#']) assert.throws(() => f.make({ url }), { code: 'INVALID_CONFIG' });
  for (const extra of [{ db: undefined }, { tracePath: 'relative' }, { actor: '' }, { maxCalls: '3' }, { maxTotalBytes: Infinity }, { timeoutMs: 15001 }, { maxCalls: 1.5 }, { maxCalls: 0 }]) assert.throws(() => f.make(extra));
  assert.equal(f.requests.length, 0);
});

test('UTF-8, IDs, combined body bounds, and tool-attempt budget reject before HTTP', async (t) => {
  const f = await fixture(t);
  const client = f.make({ maxCalls: 6 });
  await assert.rejects(client.recall('é'.repeat(513)), { code: 'INVALID_ARGUMENT' });
  await assert.rejects(client.get('bad'), { code: 'INVALID_ARGUMENT' });
  await assert.rejects(client.remember('s'.repeat(513), 'e'), { code: 'INVALID_ARGUMENT' });
  await assert.rejects(client.remember('s', 'e'.repeat(4097)), { code: 'INVALID_ARGUMENT' });
  await assert.rejects(client.remember('s', '\u0001'.repeat(4096)), { code: 'INVALID_ARGUMENT' });
  await assert.rejects(client.supersede(ID, ID), { code: 'INVALID_ARGUMENT' });
  await assert.rejects(client.recall('bounded'), { code: 'CALL_BUDGET' });
  assert.equal(f.requests.length, 0);
  await client.close();
});

test('cumulative byte budget reserves bounded responses before any work', async (t) => {
  const f = await fixture(t);
  const client = f.make({ maxTotalBytes: LIMITS.responseBytes });
  await assert.rejects(client.remember('s', 'e'), { code: 'BYTE_BUDGET' });
  assert.equal(f.requests.length, 0);
  await client.close();
});

test('expired sessions fail without reinitialization or mutation replay', async (t) => {
  const f = await fixture(t);
  const client = f.make();
  await client.get(ID);
  f.sessions.clear();
  await assert.rejects(client.remember('s', 'e'), (error) => error.code === 'SESSION_INVALID' && error.ambiguousWrite === false);
  const count = f.requests.length;
  await assert.rejects(client.remember('s', 'e'), { code: 'SESSION_INVALID' });
  assert.equal(f.requests.length, count);
  assert.equal(f.requests.filter((r) => r.body?.method === 'initialize').length, 1);
  await client.close();
});

test('ambiguous timeout and tool errors never retry writes; one operation may be in flight', async (t) => {
  let mutationCount = 0;
  const f = await fixture(t, ({ body, res }) => {
    if (body?.params?.name === 'ingest') { mutationCount++; setTimeout(() => res.destroy(), 100); return true; }
    return false;
  });
  const client = f.make({ timeoutMs: 40 });
  const pending = client.remember('s', 'e');
  await assert.rejects(client.get(ID), { code: 'BUSY' });
  await assert.rejects(pending, (error) => error.code === 'TIMEOUT' && error.ambiguousWrite === true && /inspect stored state/.test(error.message));
  assert.equal(mutationCount, 1);
  await client.close();
});

for (const mode of ['oversize-length', 'oversize-stream', 'wrong-id', 'deep', 'tool-error', 'redirect']) {
  test(`bounded protocol refusal: ${mode}`, async (t) => {
    const f = await fixture(t, ({ body, res, send }) => {
      if (body?.params?.name !== 'ingest') return false;
      if (mode === 'oversize-length') { res.writeHead(200, { 'content-type': 'application/json', 'content-length': '65537' }); res.end('x'); }
      if (mode === 'oversize-stream') { res.writeHead(200, { 'content-type': 'application/json' }); res.end('x'.repeat(70000)); }
      if (mode === 'wrong-id') { res.writeHead(200, { 'content-type': 'application/json' }); res.end('{"jsonrpc":"2.0","id":999,"result":{}}'); }
      if (mode === 'deep') send({ content: [{ type: 'text', text: '['.repeat(33) + '0' + ']'.repeat(33) }], isError: false });
      if (mode === 'tool-error') send({ content: [{ type: 'text', text: 'store save failed' }], isError: true });
      if (mode === 'redirect') { res.writeHead(307, { location: 'https://example.com/' }); res.end(); }
      return true;
    });
    const client = f.make();
    const expected = mode.startsWith('oversize') ? 'RESPONSE_BOUND' : mode === 'tool-error' ? 'TOOL' : mode === 'redirect' ? 'HTTP' : 'PROTOCOL';
    await assert.rejects(client.remember('s', 'e'), (error) => error.code === expected && error.ambiguousWrite === true);
    assert.equal(f.requests.filter((r) => r.body?.params?.name === 'ingest').length, 1);
    await client.close();
    const traces = await readFile(f.config.tracePath, 'utf8');
    assert(traces.includes('"event":"error"'));
  });
}

test('exclusive trace refusal happens before HTTP, preserving prior evidence', async (t) => {
  const f = await fixture(t);
  const first = f.make();
  await first.get(ID); await first.close();
  const before = await readFile(f.config.tracePath, 'utf8');
  const count = f.requests.length;
  const second = f.make();
  await assert.rejects(second.get(ID), { code: 'TRACE_IO' });
  assert.equal(f.requests.length, count);
  assert.equal(await readFile(f.config.tracePath, 'utf8'), before);
  await second.close();
});

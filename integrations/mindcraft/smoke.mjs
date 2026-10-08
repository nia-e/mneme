#!/usr/bin/env node
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createReadStream, createWriteStream } from 'node:fs';
import { mkdir, readFile, realpath, stat, writeFile } from 'node:fs/promises';
import { createServer, connect } from 'node:net';
import { isAbsolute, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
import { MnemeClient } from './mcp-client.mjs';
import { releaseDatabase } from './release.mjs';

const PROTOCOL = '2025-11-25';
const DB = 'mindcraft_trial';
const argv = process.argv.slice(2);
if (![4, 6].includes(argv.length) || argv[0] !== '--binary' || argv[2] !== '--output' ||
    !isAbsolute(argv[1]) || !isAbsolute(argv[3]) ||
    (argv.length === 6 && (argv[4] !== '--shutdown-signal' || !['SIGINT', 'SIGTERM'].includes(argv[5])))) {
  console.error('usage: node smoke.mjs --binary /absolute/mneme-mcp --output /absolute/new-output-directory [--shutdown-signal SIGINT|SIGTERM]');
  process.exitCode = 2;
} else {
  await main(argv[1], argv[3], argv[5]);
}

async function sha256(path) {
  const hash = createHash('sha256');
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest('hex');
}
async function port() {
  const server = createServer();
  await new Promise((resolve, reject) => server.once('error', reject).listen(0, '127.0.0.1', resolve));
  const result = server.address().port;
  await new Promise((resolve) => server.close(resolve));
  return result;
}
function ids(payload) {
  const result = new Set();
  const visit = (value) => {
    if (!value || typeof value !== 'object') return;
    if (typeof value.id === 'string') result.add(value.id);
    for (const child of Object.values(value)) visit(child);
  };
  visit(payload);
  return [...result];
}
function stable(node) {
  return Object.fromEntries(['id', 'summary', 'body', 'status', 'confidence', 'tags'].map((key) => [key, node[key]]));
}
function correction(loser, winnerId) {
  // Existing get/edges uses oriented_neighbors: Supersedes is navigable only
  // from loser to winner. Its incoming=true flag still proves winner -> loser.
  const edges = loser.edges.filter((edge) => edge.kind === 'supersedes' && edge.neighbor === winnerId && edge.incoming === true);
  assert.equal(edges.length, 1, 'one exact directed correction edge');
  return edges[0];
}

async function main(binary, output, shutdownSignal) {
  await mkdir(output, { recursive: false, mode: 0o700 }); // Refuse overwriting earlier evidence.
  const report = { schema: 'mindcraft-mneme-http-smoke.v1', status: 'running', binary, output, startedAt: new Date().toISOString(), checks: {}, limitations: [
    'Provider-free HTTP integration smoke; no Minecraft, model-provider API, or benchmark-quality claim.',
    'New supersede operations archive the loser; older already-demoted candidate losers are not migrated.',
    'The correction edge is inspected from the loser: get/edges orients Supersedes traversal from loser to winner only.',
    'Planned restart tests explicit operator release or the selected graceful signal. No SIGKILL/crash/power-loss recovery is claimed.',
  ] };
  report.shutdownMode = shutdownSignal ?? 'explicit-release';
  const clients = [];
  const hosts = [];
  let host;
  let watchdog;
  const save = () => writeFile(join(output, 'report.json'), `${JSON.stringify(report, null, 2)}\n`, { mode: 0o600 });
  try {
    assert((await stat(binary)).isFile(), 'explicit binary is a file');
    report.binarySha256 = await sha256(binary);
    report.clientSha256 = await sha256(fileURLToPath(new URL('./mcp-client.mjs', import.meta.url)));
    report.smokeSha256 = await sha256(fileURLToPath(import.meta.url));
    report.releaseSha256 = await sha256(fileURLToPath(new URL('./release.mjs', import.meta.url)));
    // This smoke intentionally requires a complete explicit semantic-model cache.
    // Nothing selects a user/project store or tries to populate a missing model.
    const cache = process.env.FASTEMBED_CACHE_DIR;
    assert(cache && isAbsolute(cache), 'set FASTEMBED_CACHE_DIR to the complete existing BGE-base cache');
    const modelRepo = join(cache, 'models--Xenova--bge-base-en-v1.5');
    const revision = (await readFile(join(modelRepo, 'refs/main'), 'utf8')).trim();
    assert(/^[a-f0-9]{40}$/.test(revision), 'cache revision is a pinned SHA');
    const modelFiles = ['config.json', 'tokenizer.json', 'tokenizer_config.json', 'special_tokens_map.json', 'onnx/model.onnx'];
    for (const name of modelFiles) assert((await stat(join(modelRepo, 'snapshots', revision, name))).size > 0, `complete cached ${name}`);
    report.embeddingCache = { path: cache, revision, requiredFiles: modelFiles };
    await mkdir(join(output, 'store'));
    await mkdir(join(output, 'traces'));
    const dbPath = join(output, 'store', 'episode.db');
    const httpPort = await port();
    const url = `http://127.0.0.1:${httpPort}/`;
    report.db = { selector: DB, path: dbPath };
    report.url = url;
    const hostArgs = ['--db', `${DB}=${dbPath}`, '--capability-profile', 'operator', '--http', `127.0.0.1:${httpPort}`];
    report.hostArgs = hostArgs;
    const env = { PATH: process.env.PATH ?? '/usr/bin:/bin', FASTEMBED_CACHE_DIR: cache,
      HF_HUB_OFFLINE: '1', TRANSFORMERS_OFFLINE: '1', MNEME_RERANK: '0',
      HTTP_PROXY: 'http://127.0.0.1:9', HTTPS_PROXY: 'http://127.0.0.1:9', ALL_PROXY: 'http://127.0.0.1:9', NO_PROXY: '127.0.0.1,::1',
      XDG_DATA_HOME: join(output, 'isolated-data'),
    };
    const startHost = async () => {
      const index = hosts.length + 1;
      const child = spawn(binary, hostArgs, { cwd: output, env, stdio: ['ignore', 'pipe', 'pipe'] });
      const entry = { child, index, exit: new Promise((resolve) => {
        child.once('error', (error) => resolve({ error: error.message }));
        child.once('exit', (code, signal) => resolve({ code, signal }));
      }), logBytes: 0, logExceeded: false };
      hosts.push(entry); host = entry;
      for (const [streamName, input] of [['stdout', child.stdout], ['stderr', child.stderr]]) {
        const dest = createWriteStream(join(output, `host-${index}.${streamName}.log`), { flags: 'wx', mode: 0o600 });
        entry[`${streamName}Done`] = new Promise((resolve, reject) => { dest.once('finish', resolve); dest.once('error', reject); });
        input.on('data', (chunk) => {
          entry.logBytes += chunk.length;
          if (entry.logBytes <= 1024 * 1024) dest.write(chunk);
          else { entry.logExceeded = true; child.kill('SIGKILL'); }
        });
        input.on('end', () => dest.end());
      }
      const deadline = Date.now() + 15000;
      while (Date.now() < deadline) {
        if (child.exitCode !== null || child.signalCode !== null) throw new Error(`HTTP host ${index} exited before readiness`);
        const ready = await new Promise((resolve) => {
          const socket = connect({ host: '127.0.0.1', port: httpPort });
          socket.setTimeout(200);
          const finish = (ok) => { socket.destroy(); resolve(ok); };
          socket.once('connect', () => finish(true)); socket.once('error', () => finish(false)); socket.once('timeout', () => finish(false));
        });
        if (ready) return entry;
        await delay(100);
      }
      throw new Error(`HTTP host ${index} startup timed out`);
    };
    const stopHost = async (entry, signal = 'SIGTERM', requireGraceful = false) => {
      if (entry.stopped) return;
      entry.requestedSignal = signal;
      entry.child.kill(signal);
      let outcome = await Promise.race([entry.exit, delay(requireGraceful ? 40000 : 3000).then(() => null)]);
      if (!outcome) { entry.child.kill('SIGKILL'); outcome = await Promise.race([entry.exit, delay(3000).then(() => { throw new Error('host did not exit after SIGKILL'); })]); }
      entry.outcome = outcome; entry.stopped = true;
      await Promise.all([entry.stdoutDone, entry.stderrDone]);
      assert(!entry.logExceeded, 'bounded host log');
      if (requireGraceful) assert.deepEqual(outcome, { code: 0, signal: null }, 'signal shutdown exits successfully without forced termination');
    };
    report.cleanup = [];
    const client = (actor, phase) => {
      const instance = new MnemeClient({ url, db: DB, actor, tracePath: join(output, 'traces', `${actor}-${phase}.jsonl`) });
      clients.push(instance); return instance;
    };
    const releaseBeforeStop = async (entry, fencedClient, knownId) => {
      await delay(1100); // Respect the existing cold-operation interval, without retry.
      const result = await releaseDatabase({ url, db: DB, trace: join(output, 'traces', `operator-release-${entry.index}.jsonl`) });
      assert.equal(result.db_id, report.databaseIdentity.db_id);
      assert.equal(await realpath(result.resolved_path), await realpath(dbPath));
      for (const suffix of ['-wal', '-shm', '-journal']) {
        await assert.rejects(stat(dbPath + suffix), { code: 'ENOENT' }, `released SQLite has no ${suffix} sidecar`);
      }
      await assert.rejects(fencedClient.get(knownId), (error) => error.code === 'TOOL' && /released for offline maintenance/.test(error.message));
      entry.released = true;
      report.checks[`host${entry.index}ExplicitReleaseAndFence`] = true;
    };
    watchdog = setTimeout(() => { for (const entry of hosts) if (!entry.stopped) entry.child.kill('SIGKILL'); }, 120000);
    await startHost();
    host.stop = () => stopHost(hosts[0]);

    // Narrow raw protocol probe covers catalog and server refusal without adding
    // a general raw-tool escape hatch to MnemeClient.
    let probeSession;
    let probeId = 0;
    const protocolTrace = [];
    const probe = async (method, params) => {
      assert(probeId < 5, 'finite protocol probe count');
      const notification = method === 'notifications/initialized';
      const request = { jsonrpc: '2.0', ...(!notification && { id: probeId + 1 }), method, params };
      probeId++;
      const headers = { Accept: 'application/json, text/event-stream', 'Content-Type': 'application/json' };
      if (probeSession) { headers['Mcp-Session-Id'] = probeSession; headers['Mcp-Protocol-Version'] = PROTOCOL; }
      const response = await fetch(url, { method: 'POST', headers, body: JSON.stringify(request), redirect: 'error', signal: AbortSignal.timeout(15000) });
      assert.equal(response.status, notification ? 202 : 200);
      let text = '', bytes = 0;
      for await (const chunk of response.body) { bytes += chunk.length; assert(bytes <= 65536, 'bounded protocol probe'); text += Buffer.from(chunk).toString('utf8'); }
      const payload = notification ? null : JSON.parse(text);
      probeSession ??= response.headers.get('mcp-session-id');
      protocolTrace.push({ request, response: payload, responseBytes: bytes });
      await writeFile(join(output, 'protocol.json'), `${JSON.stringify(protocolTrace, null, 2)}\n`, { mode: 0o600 });
      return payload?.result;
    };
    const init = await probe('initialize', { protocolVersion: PROTOCOL, capabilities: {}, clientInfo: { name: 'mindcraft-smoke-probe', version: '1' } });
    report.serverInfo = init.serverInfo;
    assert.equal(init.serverInfo.name, 'mneme-mcp');
    assert.equal(init.serverInfo.capabilityProfile, 'operator');
    await probe('notifications/initialized', {});
    const catalog = await probe('tools/list', {});
    for (const name of ['recall_context', 'get', 'ingest', 'supersede']) assert(catalog.tools.some((tool) => tool.name === name), `${name} is discoverable`);
    const databaseResult = await probe('tools/call', { name: 'databases', arguments: {} });
    assert.equal(databaseResult.isError, false);
    const databases = JSON.parse(databaseResult.content[0].text);
    assert.equal(databases.length, 1, 'host exposes only the disposable episode database');
    assert.equal(databases[0].db, DB);
    assert.equal(await realpath(databases[0].resolved_path), await realpath(dbPath));
    report.databaseIdentity = databases[0];
    const refusal = await probe('tools/call', { name: 'get', arguments: { db: DB, id: 'invalid' } });
    assert.equal(refusal.isError, true);
    const deleted = await fetch(url, { method: 'DELETE', headers: { 'Mcp-Session-Id': probeSession, 'Mcp-Protocol-Version': PROTOCOL }, redirect: 'error', signal: AbortSignal.timeout(15000) });
    assert.equal(deleted.status, 204);
    report.checks.catalogAndRefusal = true;

    const scout = client('scout', 'initial');
    const builder = client('builder', 'initial');
    let newcomer = client('newcomer', 'initial');
    const lessonText = 'The east tunnel floods when its gravel wall is opened. Keep the gravel wall sealed.';
    const lessonEvidence = 'At the east tunnel, scout broke one gravel block and saw water enter the passage; scout replaced the block.';
    const promiseText = 'Builder promised to finish the starter house roof before sunset.';
    const promiseEvidence = 'Builder explicitly promised the village a finished starter house roof before sunset; this remains outstanding.';
    const lesson = await scout.remember(lessonText, lessonEvidence);
    const promise = await builder.remember(promiseText, promiseEvidence);
    assert(lesson.id && promise.id && lesson.id !== promise.id);
    report.ids = { lesson: lesson.id, promise: promise.id };
    assert(ids(await builder.recall('east tunnel gravel wall flooding')).includes(lesson.id), 'builder shares scout observation');
    assert(ids(await newcomer.recall('starter house roof promise before sunset')).includes(promise.id), 'newcomer shares promise');
    const original = await newcomer.get(lesson.id);
    const promised = await newcomer.get(promise.id);
    assert.equal(JSON.parse(original.body).evidence, lessonEvidence);
    assert.equal(JSON.parse(original.body).observer, 'scout');
    assert.equal(original.status, 'active');
    assert.deepEqual(original.tags, ['mindcraft']);
    report.checks.sharedObservationPromiseAndEvidence = true;

    await newcomer.close();
    newcomer = client('newcomer', 'reset');
    assert(ids(await newcomer.recall('east tunnel gravel flooding')).includes(lesson.id), 'fresh client recalls after reset');
    const replacement = await newcomer.remember('The east tunnel water source is now blocked with stone; the tested passage stays dry.',
      'Newcomer observed scout place stone at the east tunnel water source and reopen the passage; no water entered during the subsequent check.');
    report.ids.replacement = replacement.id;
    await newcomer.supersede(replacement.id, lesson.id);
    const archived = await scout.get(lesson.id);
    const winner = await builder.get(replacement.id);
    assert.equal(archived.status, 'archived');
    assert.equal(archived.confidence, original.confidence / 2);
    assert.equal(archived.body, original.body);
    const edge = correction(archived, replacement.id);
    await delay(1100); // Existing host cold-operation admission interval, not a retry loop.
    await newcomer.supersede(replacement.id, lesson.id); // Intentional exact explicit retry.
    assert.deepEqual(stable(await scout.get(lesson.id)), stable(archived), 'retry does not apply archival or decay twice');
    assert.deepEqual(correction(await builder.get(lesson.id), replacement.id), edge);
    assert.deepEqual(stable(await newcomer.get(promise.id)), stable(promised), 'unrelated promise retained');
    const revisedRecall = await newcomer.recall('east tunnel gravel wall flooding dry stone');
    report.revisedRecall = revisedRecall;
    report.oldAdviceStillRecalled = ids(revisedRecall).includes(lesson.id);
    assert.equal(report.oldAdviceStillRecalled, false, 'archived correction stays out of ordinary recall');
    report.checks.newcomerResetExplicitRevisionAndExactRetry = true;
    const saved = { ids: report.ids, nodes: { lesson: stable(archived), promise: stable(promised), replacement: stable(winner) }, correction: edge };
    await writeFile(join(output, 'saved-state.json'), `${JSON.stringify(saved, null, 2)}\n`, { mode: 0o600 });
    await builder.close(); await newcomer.close();
    if (shutdownSignal) {
      report.walBytesBeforeSignal = (await stat(dbPath + '-wal')).size;
      assert(report.walBytesBeforeSignal > 0, 'signal regression starts with an uncheckpointed WAL');
      await stopHost(host, shutdownSignal, true);
      for (const suffix of ['-wal', '-shm', '-journal']) {
        await assert.rejects(stat(dbPath + suffix), { code: 'ENOENT' }, `signal shutdown leaves no ${suffix} sidecar`);
      }
      report.checks.signalShutdownCheckpointAndExit = true;
    } else {
      await releaseBeforeStop(host, scout, lesson.id);
      await stopHost(host);
    }
    await startHost();
    host.stop = () => stopHost(hosts[1]);
    await assert.rejects(scout.recall('east tunnel'), (error) => error.code === 'SESSION_INVALID' && error.ambiguousWrite === false);
    await assert.rejects(scout.recall('east tunnel'), (error) => error.code === 'SESSION_INVALID');
    const restarted = client('newcomer', 'restart');
    const fromDisk = JSON.parse(await readFile(join(output, 'saved-state.json'), 'utf8'));
    for (const [name, nodeId] of Object.entries(fromDisk.ids)) assert.deepEqual(stable(await restarted.get(nodeId)), fromDisk.nodes[name]);
    assert.deepEqual(correction(await restarted.get(lesson.id), replacement.id), fromDisk.correction);
    await restarted.supersede(replacement.id, lesson.id);
    assert.deepEqual(stable(await restarted.get(lesson.id)), fromDisk.nodes.lesson, 'exact retry also stays idempotent after restart');
    assert(ids(await restarted.recall('starter house roof promise before sunset')).includes(promise.id));
    assert(!ids(await restarted.recall('east tunnel gravel wall flooding dry stone')).includes(lesson.id), 'archived advice stays excluded after restart');
    report.checks.restartPersistedIdsBodiesEdgePromiseAndSessionFence = true;
    await releaseBeforeStop(host, restarted, lesson.id);
    await restarted.close();
    if (shutdownSignal) {
      await stopHost(host, shutdownSignal, true);
      report.checks.signalShutdownAlreadyReleasedStore = true;
    }
    report.status = 'passed';
  } catch (error) {
    report.status = 'failed';
    report.error = { name: error.name, code: error.code, message: error.message, stack: error.stack };
    process.exitCode = 1;
  } finally {
    clearTimeout(watchdog);
    report.cleanup ??= [];
    for (const instance of clients) {
      try { await instance.close(); } catch (error) { report.cleanup.push({ clientError: error.message }); }
    }
    for (const entry of hosts) {
      try {
        if (entry.stop) await entry.stop();
        else if (!entry.stopped) {
          entry.child.kill('SIGKILL');
          entry.outcome = await Promise.race([entry.exit, delay(3000).then(() => ({ error: 'exit timeout' }))]);
        }
        report.cleanup.push({ host: entry.index, outcome: entry.outcome, requestedSignal: entry.requestedSignal, logBytes: entry.logBytes, databaseReleasedBeforeStop: entry.released === true });
      } catch (error) { report.cleanup.push({ host: entry.index, error: error.message }); report.status = 'failed'; process.exitCode = 1; }
    }
    report.finishedAt = new Date().toISOString();
    await save();
    console.log(JSON.stringify({ status: report.status, report: join(output, 'report.json'), checks: report.checks, error: report.error?.message }));
  }
}

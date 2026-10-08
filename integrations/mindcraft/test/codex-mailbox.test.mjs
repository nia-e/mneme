import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { lstat, mkdir, mkdtemp, readFile, readdir, realpath, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { CodexMailbox, DEFAULTS, respond } from '../codex-mailbox.mjs';

const exec = promisify(execFile);
const modulePath = fileURLToPath(new URL('../codex-mailbox.mjs', import.meta.url));
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
async function fixture(t, limits = {}) {
    const directory = await realpath(await mkdtemp(join(tmpdir(), 'codex-mailbox-test-')));
    t.after(() => rm(directory, { recursive: true, force: true }));
    const params = { mailboxDir: directory, requestTimeoutMs: 1000, ...limits };
    return { directory, params, provider: new CodexMailbox('Alice', undefined, params) };
}
async function requests(directory, count = 1) {
    const end = Date.now() + 2000;
    while (Date.now() < end) {
        const files = (await readdir(directory)).filter(name => name.endsWith('.request.json'));
        if (files.length >= count) return Promise.all(files.map(async name => ({ path: join(directory, name), packet: JSON.parse(await readFile(join(directory, name), 'utf8')) })));
        await delay(2);
    }
    throw new Error('Test request was not published');
}
async function answer(directory, request, content) {
    const file = join(directory, `${request.packet.id}.answer.txt`);
    await writeFile(file, content, { flag: 'wx' });
    return respond(request.path, file);
}
async function events(directory) {
    const traces = (await readdir(directory)).filter(name => name.startsWith('.trace-'));
    return (await Promise.all(traces.map(name => readFile(join(directory, name), 'utf8')))).flatMap(raw => raw.trim().split('\n').filter(Boolean).map(line => JSON.parse(line)));
}

test('unconfigured construction and embedding do no provider work', async () => {
    const provider = new CodexMailbox();
    assert.equal(CodexMailbox.prefix, 'codex-mailbox');
    await assert.rejects(provider.embed('example'), /embeddings are disabled/);
    await assert.rejects(provider.sendRequest([], 'prompt'), /actor must match/);
    assert.equal(DEFAULTS.maxRequests, 10);
    assert.equal(DEFAULTS.requestTimeoutMs, 120000);
});

test('exact prompts and turns pass through atomic readonly packets and bounded traces', async t => {
    const { provider, directory } = await fixture(t);
    const turns = [{ role: 'user', content: 'private-prompt-data: inspect water, not old advice.' }];
    const system = 'Actual Mindcraft system\n$ substitutions already expanded.';
    const pending = provider.sendRequest(turns, system);
    const [request] = await requests(directory);
    assert.equal(request.packet.actor, 'Alice');
    assert.equal(request.packet.systemMessage, system);
    assert.deepEqual(request.packet.turns, turns);
    assert.equal(request.packet.maxResponseBytes, 8192);
    const original = await readFile(request.path);
    const result = await answer(directory, request, '!recallExperience("northern tunnel")');
    assert.equal(await pending, '!recallExperience("northern tunnel")');
    assert.deepEqual(await readFile(request.path), original);
    assert.equal((await lstat(request.path)).mode & 0o222, 0);
    assert.equal((await lstat(result.responsePath)).mode & 0o222, 0);
    const trace = await events(directory);
    assert.deepEqual(trace.map(e => e.status), ['requested', 'answered']);
    assert.equal(trace[1].requestBytes, original.byteLength);
    assert.equal(trace[1].responseBytes, (await readFile(result.responsePath)).byteLength);
    assert.ok(trace[1].durationMs >= 0);
    assert.equal(JSON.stringify(trace).includes('private-prompt-data'), false);
    assert.equal((await readdir(directory)).some(name => name.endsWith('.tmp') || name.endsWith('.lock')), false);
});

test('overlapping conversation and native summary use distinct IDs and share a synchronous quota', async t => {
    const { provider, directory } = await fixture(t, { maxRequests: 2 });
    const first = provider.sendRequest([{ role: 'user', content: 'work' }], 'conversation');
    const second = provider.sendRequest([], 'native summary: preserve useful observations');
    await assert.rejects(provider.sendRequest([], 'third'), /quota exhausted/);
    const all = await requests(directory, 2);
    const summary = all.find(request => request.packet.turns.length === 0);
    const conversation = all.find(request => request !== summary);
    assert.notEqual(summary.packet.id, conversation.packet.id);
    await answer(directory, summary, 'The tunnel is wet; preserve the starter house.');
    assert.equal(await second, 'The tunnel is wet; preserve the starter house.');
    await answer(directory, conversation, '!nearbyBlocks');
    assert.equal(await first, '!nearbyBlocks');
    assert.equal((await requests(directory, 2)).length, 2);
    assert.deepEqual((await events(directory)).filter(e => e.status === 'answered').map(e => e.attempt), [2, 1]);
});

test('invalid requests consume quota without publishing a prompt', async t => {
    const { provider, directory } = await fixture(t, { maxRequests: 1 });
    await assert.rejects(provider.sendRequest([{ role: 'user', content: 42 }], 'prompt'), /Each turn/);
    await assert.rejects(provider.sendRequest([], 'valid'), /quota exhausted/);
    assert.equal((await readdir(directory)).filter(name => name.endsWith('.request.json')).length, 0);
});

test('prepublication input/configuration bounds and actor identity are strict', async t => {
    const { directory } = await fixture(t);
    for (const limits of [{ maxRequests: 101 }, { requestTimeoutMs: 300001 }, { maxRequestBytes: 1048577 }, { maxResponseBytes: 32769 }, { maxRequests: 1.5 }]) {
        await assert.rejects(new CodexMailbox('Alice', undefined, { mailboxDir: directory, ...limits }).sendRequest([], 'prompt'), /must be an integer/);
    }
    await assert.rejects(new CodexMailbox('Alice', undefined, { mailboxDir: directory, actor: 'Bob' }).sendRequest([], 'prompt'), /actor must match/);
    await assert.rejects(new CodexMailbox('Alice', undefined, { mailboxDir: directory, maxRequestBytes: 300 }).sendRequest([], 'é'.repeat(200)), /byte limit/);
    await assert.rejects(new CodexMailbox('Alice', undefined, { mailboxDir: directory }).sendRequest([], '\ud800'), /well-formed/);
    await assert.rejects(new CodexMailbox('Alice', undefined, { mailboxDir: directory }).sendRequest([{ role: 'user', content: 'a', extra: {} }], 'p'), /Each turn/);
    assert.equal((await readdir(directory)).filter(name => name.endsWith('.request.json')).length, 0);
});

test('a wrong response ID or oversized response rejects without fabricated completion', async t => {
    for (const mode of ['wrong-id', 'oversized', 'malformed']) {
        const { provider, directory } = await fixture(t, { maxResponseBytes: 128 });
        const pending = provider.sendRequest([], 'prompt');
        const rejection = assert.rejects(pending, mode === 'wrong-id' ? /matching id/ : mode === 'oversized' ? /byte limit/ : /JSON/);
        const [request] = await requests(directory);
        const raw = mode === 'wrong-id' ? JSON.stringify({ id: 'wrong', content: '!stop' }) : mode === 'oversized' ? 'x'.repeat(129) : '{broken';
        await writeFile(join(directory, `${request.packet.id}.response.json`), raw);
        await rejection;
        assert.equal((await events(directory)).at(-1).status, 'failed');
    }
});

test('timeout records failure and helper refuses a late answer', async t => {
    const { provider, directory } = await fixture(t, { requestTimeoutMs: 40 });
    const pending = provider.sendRequest([], 'prompt');
    const rejection = assert.rejects(pending, /timed out/);
    const [request] = await requests(directory);
    await rejection;
    await assert.rejects(answer(directory, request, '!stop'), /expired/);
    assert.equal((await events(directory)).at(-1).status, 'failed');
});

test('helper refuses duplicate/concurrent outputs and enforces encoded reply bound', async t => {
    const { provider, directory } = await fixture(t, { maxResponseBytes: 128 });
    const pending = provider.sendRequest([], 'prompt');
    const [request] = await requests(directory);
    const oversized = join(directory, 'oversized.txt');
    await writeFile(oversized, 'x'.repeat(120));
    await assert.rejects(respond(request.path, oversized), /Encoded reply exceeds/);
    const file = join(directory, 'answer.txt');
    await writeFile(file, '!nearbyBlocks');
    const outcomes = await Promise.allSettled([respond(request.path, file), respond(request.path, file)]);
    assert.equal(outcomes.filter(result => result.status === 'fulfilled').length, 1);
    assert.equal(outcomes.filter(result => result.status === 'rejected').length, 1);
    assert.equal(await pending, '!nearbyBlocks');
    await assert.rejects(respond(request.path, file), /already exists/);
});

test('mailbox directory, request and response symlinks are refused', async t => {
    const { directory } = await fixture(t);
    const real = join(directory, 'real');
    await mkdir(real);
    const alias = join(directory, 'alias');
    await symlink(real, alias);
    await assert.rejects(new CodexMailbox('Alice', undefined, { mailboxDir: alias }).sendRequest([], 'prompt'), /symlink/);
    const { provider, directory: mailbox } = await fixture(t);
    const pending = provider.sendRequest([], 'prompt');
    const rejection = assert.rejects(pending, /ELOOP|symbolic/i);
    const [request] = await requests(mailbox);
    const textPath = join(mailbox, 'text.txt');
    await writeFile(textPath, JSON.stringify({ id: request.packet.id, content: '!stop' }));
    await symlink(textPath, join(mailbox, `${request.packet.id}.response.json`));
    await rejection;
    const requestAlias = join(mailbox, 'alias.request.json');
    await symlink(request.path, requestAlias);
    await assert.rejects(respond(requestAlias, textPath), /ELOOP|symbolic/i);
});

test('CLI helper submits plain actor text and runtime reconfiguration requires a new instance', async t => {
    const { provider, directory, params } = await fixture(t);
    const pending = provider.sendRequest([], 'prompt');
    const [request] = await requests(directory);
    const answerPath = join(directory, 'actor-answer.txt');
    await writeFile(answerPath, '!inventory');
    const { stdout } = await exec(process.execPath, [modulePath, 'respond', request.path, answerPath]);
    assert.equal(JSON.parse(stdout).id, request.packet.id);
    assert.equal(await pending, '!inventory');
    params.maxRequests = 2;
    await assert.rejects(provider.sendRequest([], 'next'), /configuration changed/);
    await assert.rejects(exec(process.execPath, [modulePath, 'respond', request.path]), /Usage/);
});

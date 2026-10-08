import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, isAbsolute, join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { createContext, SourceTextModule, SyntheticModule } from 'node:vm';
import test from 'node:test';
import { installMindcraft, parseArguments, RUNTIME_DIRECTORY, sha256, STATE_DIRECTORY, transformHistory, transformRegistry, upstream } from '../install.mjs';
import { nativeMemoryEnabled } from '../memory-commands.mjs';

const checkout = process.env.MINDCRAFT_CHECKOUT;
assert.ok(checkout && isAbsolute(checkout), 'Set MINDCRAFT_CHECKOUT to the explicit pinned Mindcraft checkout.');
const original = Object.fromEntries(Object.keys(upstream.files).map(name => [name, readFileSync(join(checkout, name))]));
for (const [name, bytes] of Object.entries(original)) assert.equal(sha256(bytes), upstream.files[name]);

function fixture(t) {
    const temporary = realpathSync(mkdtempSync(join(tmpdir(), 'mindcraft-install-test-')));
    t.after(() => rmSync(temporary, { recursive: true, force: true }));
    const target = join(temporary, 'checkout');
    // Local object sharing only; no network and no upstream dependency install.
    execFileSync('git', ['clone', '--shared', '--no-checkout', '--quiet', '--', checkout, target]);
    for (const [name, bytes] of Object.entries(original)) {
        mkdirSync(dirname(join(target, name)), { recursive: true });
        writeFileSync(join(target, name), bytes);
    }
    return { target, temporary };
}

test('installer preserves pinned originals, records runtime identities and checks an exact repeat', async t => {
    const { target } = fixture(t);
    const before = installMindcraft(target, { check: true });
    assert.equal(before.state, 'not_installed');
    assert.equal(existsSync(join(target, STATE_DIRECTORY)), false);
    const installed = installMindcraft(target);
    assert.equal(installed.state, 'installed');
    assert.equal(installed.changed, true);
    for (const [name, bytes] of Object.entries(original)) {
        assert.deepEqual(readFileSync(join(target, STATE_DIRECTORY, 'originals', name)), bytes);
        assert.notDeepEqual(readFileSync(join(target, name)), bytes);
    }
    for (const [name, hash] of Object.entries(installed.files)) assert.equal(sha256(readFileSync(join(target, name))), hash);
    const manifest = readFileSync(join(target, STATE_DIRECTORY, 'manifest.json'));
    assert.equal(installMindcraft(target).changed, false);
    assert.equal(installMindcraft(target, { check: true }).state, 'installed');
    assert.deepEqual(readFileSync(join(target, STATE_DIRECTORY, 'manifest.json')), manifest);
    const copied = await import(pathToFileURL(join(target, RUNTIME_DIRECTORY, 'memory-commands.mjs')).href);
    assert.equal(copied.mnemeCommands.length, 4);
    assert.equal(copied.mnemeEnabled({ prompter: { profile: { memory_condition: 'mneme' } } }), true);
    assert.match(await copied.mnemeCommands[0].perform({ prompter: { profile: { memory_condition: 'empty' } } }, 'water'), /disabled/);
});

test('source and HEAD drift are rejected before any install writes', t => {
    const { target } = fixture(t);
    const registry = join(target, 'src/agent/commands/index.js');
    const drift = Buffer.concat([original['src/agent/commands/index.js'], Buffer.from('\n// local change\n')]);
    writeFileSync(registry, drift);
    assert.throws(() => installMindcraft(target), /source drift/i);
    assert.deepEqual(readFileSync(registry), drift);
    assert.equal(existsSync(join(target, STATE_DIRECTORY)), false);
    writeFileSync(registry, original['src/agent/commands/index.js']);
    writeFileSync(join(target, '.git/HEAD'), `${'a'.repeat(40)}\n`);
    assert.throws(() => installMindcraft(target), /HEAD must/);
    assert.equal(existsSync(join(target, STATE_DIRECTORY)), false);
});

test('partial installation, changed installed bytes and changed originals are never repaired implicitly', t => {
    const partial = fixture(t).target;
    mkdirSync(join(partial, STATE_DIRECTORY));
    assert.throws(() => installMindcraft(partial), /Partial or changed/);
    assert.deepEqual(readFileSync(join(partial, 'src/agent/history.js')), original['src/agent/history.js']);
    const runtimeOnly = fixture(t).target;
    mkdirSync(join(runtimeOnly, RUNTIME_DIRECTORY));
    assert.throws(() => installMindcraft(runtimeOnly), /Partial.*installation/);
    const modified = fixture(t).target;
    installMindcraft(modified);
    const target = join(modified, RUNTIME_DIRECTORY, 'memory-commands.mjs');
    writeFileSync(target, 'export const changed = true;\n');
    assert.throws(() => installMindcraft(modified, { check: true }), /Installed file differs/);
    assert.equal(readFileSync(target, 'utf8'), 'export const changed = true;\n');
    const backup = join(modified, STATE_DIRECTORY, 'originals/src/agent/history.js');
    writeFileSync(backup, 'changed original');
    assert.throws(() => installMindcraft(modified), /Original source drift/);
});

test('installer refuses symlink destinations and complete CLI argument ambiguity', t => {
    const { target, temporary } = fixture(t);
    const outside = join(temporary, 'outside');
    mkdirSync(outside);
    symlinkSync(outside, join(target, RUNTIME_DIRECTORY));
    assert.throws(() => installMindcraft(target), /symlink/);
    assert.equal(existsSync(join(target, STATE_DIRECTORY)), false);
    for (const args of [[], ['--mindcraft'], ['--unknown'], ['--mindcraft', target, '--check', '--check'], ['--mindcraft', target, '--mindcraft', target], ['--help', '--check']]) {
        assert.throws(() => parseArguments(args), /Usage/);
    }
    assert.deepEqual(parseArguments(['--mindcraft', target, '--check']), { target, check: true });
    assert.throws(() => installMindcraft('relative'), /absolute/);
});

test('transforms reject altered source and preserve unrelated rolling-history logic', () => {
    assert.throws(() => transformRegistry(Buffer.from('registry')), /source drift/);
    assert.throws(() => transformHistory(Buffer.from('history')), /source drift/);
    const transformed = transformHistory(original['src/agent/history.js']).toString();
    const before = original['src/agent/history.js'].toString();
    const start = before.indexOf('    async add(name, content) {');
    const end = before.indexOf('    async save() {');
    assert.ok(transformed.includes(before.slice(start, end)));
    assert.ok(transformed.includes("this.memory = nativeMemoryEnabled(this.agent) ? (data.memory || '') : '';"));
});

async function historyHarness(condition) {
    const files = new Map();
    let summaries = 0;
    const context = createContext({ console: { log() {}, error() {} } });
    const modules = {
        fs: {
            mkdirSync() {}, existsSync: name => files.has(name),
            readFileSync: name => { if (!files.has(name)) throw new Error('Missing test file'); return files.get(name); },
            writeFileSync: (name, value) => files.set(name, value),
        },
        './npc/data.js': { NPCData: class {} },
        './settings.js': { default: { max_messages: 6 } },
        './commands/mneme/memory-commands.mjs': { nativeMemoryEnabled },
    };
    const module = new SourceTextModule(transformHistory(original['src/agent/history.js']).toString(), { context });
    await module.link(specifier => {
        assert.ok(modules[specifier], `Unexpected history import ${specifier}`);
        return new SyntheticModule(Object.keys(modules[specifier]), function () {
            for (const [name, value] of Object.entries(modules[specifier])) this.setExport(name, value);
        }, { context });
    });
    await module.evaluate();
    const profile = {};
    if (condition !== undefined) profile.memory_condition = condition;
    const agent = {
        name: 'Ada', prompter: { profile, promptMemSaving: async () => { summaries++; return 'native tunnel warning'; } },
        self_prompter: { state: 0, isStopped: () => true }, task: { taskStartTime: 123 }, last_sender: null,
    };
    const history = new module.namespace.History(agent);
    return { history, agent, files, summaries: () => summaries };
}

test('actual pinned History suppresses native summary generation/load only for Mneme and empty', async () => {
    for (const condition of ['mneme', 'empty', 'native', undefined]) {
        const h = await historyHarness(condition);
        const enabled = condition === 'native' || condition === undefined;
        h.files.set(h.history.memory_fp, JSON.stringify({ memory: 'predecessor summary', turns: [{ role: 'user', content: 'old turn' }], self_prompt: 'old goal' }));
        h.history.load();
        assert.equal(h.history.memory, enabled ? 'predecessor summary' : '');
        // Loading still restores recent turns: phase-reset owns their removal.
        assert.equal(h.history.turns.length, 1);
        h.history.clear();
        for (let i = 0; i < 6; i++) await h.history.add('player', `observation ${i}`);
        assert.equal(h.history.turns.length, 1);
        assert.equal(h.summaries(), enabled ? 1 : 0);
        assert.equal(h.history.memory, enabled ? 'native tunnel warning' : '');
        const archived = JSON.parse(h.files.get(h.history.full_history_fp));
        assert.equal(archived.length, 5);
        await h.history.save();
        assert.equal(JSON.parse(h.files.get(h.history.memory_fp)).memory, enabled ? 'native tunnel warning' : '');
    }
});

test('unknown memory condition fails before loading or generating native summaries', async () => {
    const h = await historyHarness('plain-notes');
    h.files.set(h.history.memory_fp, JSON.stringify({ memory: 'hidden advantage', turns: [] }));
    assert.throws(() => h.history.load(), /memory_condition/);
    await assert.rejects(h.history.summarizeMemories([]), /memory_condition/);
    assert.equal(h.summaries(), 0);
});

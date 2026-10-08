#!/usr/bin/env node
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { existsSync, lstatSync, mkdirSync, readFileSync, realpathSync, renameSync, writeFileSync } from 'node:fs';
import { dirname, isAbsolute, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const sourceDir = dirname(fileURLToPath(import.meta.url));
export const upstream = JSON.parse(readFileSync(join(sourceDir, 'upstream.json'), 'utf8'));
export const STATE_DIRECTORY = '.mneme-adapter';
export const RUNTIME_DIRECTORY = 'src/agent/commands/mneme';
const REGISTRY = 'src/agent/commands/index.js';
const HISTORY = 'src/agent/history.js';
const RUNTIME_FILES = ['memory-commands.mjs', 'mcp-client.mjs'];

export function sha256(bytes) {
    return createHash('sha256').update(bytes).digest('hex');
}

function replaceOnce(source, before, after, label) {
    if (source.split(before).length !== 2) throw new Error(`Cannot locate the unique pinned ${label} hook.`);
    return source.replace(before, after);
}

// Transform only the exact pinned bytes. Tests apply these same transforms to the
// real upstream modules rather than to a parallel implementation of their APIs.
export function transformRegistry(bytes) {
    if (sha256(bytes) !== upstream.files[REGISTRY]) throw new Error('Mindcraft command registry source drift; use the pinned checkout.');
    let source = bytes.toString('utf8');
    source = replaceOnce(source,
        "import { queryList } from './queries.js';",
        "import { queryList } from './queries.js';\nimport { mnemeCommands, mnemeEnabled } from './mneme/memory-commands.mjs';", 'registry import');
    source = replaceOnce(source,
        'const commandList = queryList.concat(actionsList);',
        'const commandList = queryList.concat(actionsList, mnemeCommands);', 'registry list');
    source = replaceOnce(source,
        '    for (let command of commandList) {\n        if (agent.blocked_actions.includes(command.name)) {',
        '    for (let command of commandList) {\n        if (mnemeCommands.includes(command) && !mnemeEnabled(agent)) continue;\n        if (agent.blocked_actions.includes(command.name)) {', 'command discovery');
    return Buffer.from(source);
}

export function transformHistory(bytes) {
    if (sha256(bytes) !== upstream.files[HISTORY]) throw new Error('Mindcraft history source drift; use the pinned checkout.');
    let source = bytes.toString('utf8');
    source = replaceOnce(source,
        "import settings from './settings.js';",
        "import settings from './settings.js';\nimport { nativeMemoryEnabled } from './commands/mneme/memory-commands.mjs';", 'history import');
    source = replaceOnce(source,
        '    async summarizeMemories(turns) {\n',
        '    async summarizeMemories(turns) {\n        if (!nativeMemoryEnabled(this.agent)) {\n            this.memory = \'\';\n            return;\n        }\n', 'native summary admission');
    source = replaceOnce(source,
        "            this.memory = data.memory || '';",
        "            this.memory = nativeMemoryEnabled(this.agent) ? (data.memory || '') : '';", 'native summary loading');
    return Buffer.from(source);
}

function checkedPath(root, relative, { missing = false, directory = false } = {}) {
    let current = root;
    const parts = relative.split('/');
    for (let index = 0; index < parts.length; index++) {
        current = join(current, parts[index]);
        if (!existsSync(current)) {
            // existsSync follows links, so distinguish a dangling link as well.
            try { lstatSync(current); } catch (error) {
                if (error.code === 'ENOENT' && missing) return join(root, ...parts);
                throw error;
            }
        }
        const stat = lstatSync(current);
        if (stat.isSymbolicLink()) throw new Error(`Refusing symlink in Mindcraft adapter path: ${relative}`);
        if (index < parts.length - 1 || directory) {
            if (!stat.isDirectory()) throw new Error(`Expected directory at Mindcraft adapter path: ${relative}`);
        } else if (!stat.isFile()) {
            throw new Error(`Expected regular file at Mindcraft adapter path: ${relative}`);
        }
    }
    return current;
}

function targetRoot(target) {
    if (typeof target !== 'string' || !isAbsolute(target)) throw new Error('--mindcraft requires an absolute checkout directory.');
    const root = resolve(target);
    if (!lstatSync(root).isDirectory() || realpathSync(root) !== root) throw new Error('Mindcraft checkout must be a real directory without symlink aliases.');
    let revision;
    try {
        const top = execFileSync('git', ['-C', root, 'rev-parse', '--show-toplevel'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
        if (realpathSync(top) !== root) throw new Error('Target is not the checkout root.');
        revision = execFileSync('git', ['-C', root, 'rev-parse', 'HEAD'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
    } catch {
        throw new Error('Mindcraft installation requires a Git checkout at the pinned revision.');
    }
    if (revision !== upstream.revision) throw new Error(`Mindcraft HEAD must be ${upstream.revision}; source revision differs.`);
    return root;
}

function expectedInstallation(root, adapterDir) {
    const originals = {};
    const patched = {};
    const state = join(root, STATE_DIRECTORY);
    const hasState = existsSync(state);
    for (const path of [REGISTRY, HISTORY]) {
        const sourcePath = hasState ? `${STATE_DIRECTORY}/originals/${path}` : path;
        const bytes = readFileSync(checkedPath(root, sourcePath));
        if (sha256(bytes) !== upstream.files[path]) throw new Error(`Original source drift at ${path}; use a fresh pinned checkout.`);
        originals[path] = bytes;
        patched[path] = path === REGISTRY ? transformRegistry(bytes) : transformHistory(bytes);
    }
    const runtime = {};
    for (const name of RUNTIME_FILES) {
        const source = join(adapterDir, name);
        if (!lstatSync(source).isFile() || lstatSync(source).isSymbolicLink()) throw new Error(`Adapter source must be a regular file: ${name}`);
        runtime[`${RUNTIME_DIRECTORY}/${name}`] = readFileSync(source);
    }
    const files = Object.fromEntries(Object.entries({ ...patched, ...runtime }).map(([name, bytes]) => [name, sha256(bytes)]));
    const manifest = { schema: 'mneme.mindcraft-install.v1', upstream, files };
    return { originals, patched, runtime, manifest };
}

function checkExisting(root, expected) {
    const stateExists = existsSync(join(root, STATE_DIRECTORY));
    const runtimeExists = existsSync(join(root, RUNTIME_DIRECTORY));
    if (!stateExists && !runtimeExists) return false;
    if (!stateExists || !runtimeExists) throw new Error('Partial Mindcraft adapter installation; inspect preserved originals and use a fresh pinned checkout.');
    checkedPath(root, STATE_DIRECTORY, { directory: true });
    checkedPath(root, RUNTIME_DIRECTORY, { directory: true });
    try {
        const manifest = JSON.parse(readFileSync(checkedPath(root, `${STATE_DIRECTORY}/manifest.json`), 'utf8'));
        if (JSON.stringify(manifest) !== JSON.stringify(expected.manifest)) throw new Error('Manifest or adapter source identity differs.');
        for (const [name, hash] of Object.entries(expected.manifest.files)) {
            if (sha256(readFileSync(checkedPath(root, name))) !== hash) throw new Error(`Installed file differs: ${name}`);
        }
    } catch (error) {
        throw new Error(`Partial or changed Mindcraft adapter installation: ${error.message} Inspect preserved originals; use a fresh pinned checkout.`);
    }
    return true;
}

export function installMindcraft(target, { check = false, adapterDir = sourceDir } = {}) {
    const root = targetRoot(target);
    // Validate destination parents even before first-time creation; do not follow
    // a symlink out of the explicitly supplied installation.
    checkedPath(root, STATE_DIRECTORY, { missing: true, directory: true });
    checkedPath(root, RUNTIME_DIRECTORY, { missing: true, directory: true });
    for (const name of [REGISTRY, HISTORY]) checkedPath(root, name);
    let expected;
    try {
        expected = expectedInstallation(root, adapterDir);
    } catch (error) {
        if (existsSync(join(root, STATE_DIRECTORY)) || existsSync(join(root, RUNTIME_DIRECTORY))) {
            throw new Error(`Partial or changed Mindcraft adapter installation: ${error.message} Inspect preserved originals; use a fresh pinned checkout.`);
        }
        throw error;
    }
    if (checkExisting(root, expected)) return { state: 'installed', changed: false, root, ...expected.manifest };
    if (check) return { state: 'not_installed', changed: false, root, ...expected.manifest };

    const state = join(root, STATE_DIRECTORY);
    mkdirSync(state); // Exclusive creation: an interrupted installation is evidence.
    try {
        for (const [name, bytes] of Object.entries(expected.originals)) {
            const backup = join(state, 'originals', name);
            mkdirSync(dirname(backup), { recursive: true });
            writeFileSync(backup, bytes, { flag: 'wx' });
        }
        mkdirSync(join(root, RUNTIME_DIRECTORY));
        for (const [name, bytes] of Object.entries(expected.runtime)) writeFileSync(join(root, name), bytes, { flag: 'wx' });
        for (const [name, bytes] of Object.entries(expected.patched)) {
            const target = join(root, name);
            if (sha256(readFileSync(checkedPath(root, name))) !== upstream.files[name]) throw new Error(`Source changed during installation: ${name}`);
            const temporary = `${target}.mneme-install-new`;
            writeFileSync(temporary, bytes, { flag: 'wx', mode: lstatSync(target).mode & 0o777 });
            renameSync(temporary, target);
        }
        writeFileSync(join(state, 'manifest.json'), `${JSON.stringify(expected.manifest, null, 2)}\n`, { flag: 'wx' });
    } catch (error) {
        throw new Error(`Installation interrupted: ${error.message}. Originals remain in ${STATE_DIRECTORY}/originals; inspect them and use a fresh pinned checkout.`);
    }
    checkExisting(root, expected);
    return { state: 'installed', changed: true, root, ...expected.manifest };
}

export function parseArguments(args) {
    if (args.length === 1 && args[0] === '--help') return { help: true };
    let target;
    let check = false;
    for (let i = 0; i < args.length; i++) {
        if (args[i] === '--mindcraft' && target === undefined && args[i + 1] && !args[i + 1].startsWith('--')) target = args[++i];
        else if (args[i] === '--check' && !check) check = true;
        else throw new Error('Usage: node install.mjs --mindcraft /absolute/pinned-checkout [--check]');
    }
    if (!target) throw new Error('Usage: node install.mjs --mindcraft /absolute/pinned-checkout [--check]');
    return { target, check };
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
    try {
        const args = parseArguments(process.argv.slice(2));
        if (args.help) console.log('Usage: node install.mjs --mindcraft /absolute/pinned-checkout [--check]\n--check verifies exact source/installed identities without writing. Run only while Mindcraft is stopped.');
        else console.log(JSON.stringify(installMindcraft(args.target, { check: args.check }), null, 2));
    } catch (error) {
        console.error(error.message);
        process.exitCode = 1;
    }
}

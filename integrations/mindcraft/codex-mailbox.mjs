import { constants } from 'node:fs';
import { chmod, lstat, open, realpath, rename, unlink, writeFile } from 'node:fs/promises';
import { randomUUID } from 'node:crypto';
import { basename, dirname, isAbsolute, join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { performance } from 'node:perf_hooks';

export const DEFAULTS = Object.freeze({ maxRequests: 10, requestTimeoutMs: 120000, maxRequestBytes: 262144, maxResponseBytes: 8192 });
const CAPS = { maxRequests: 100, requestTimeoutMs: 300000, maxRequestBytes: 1048576, maxResponseBytes: 32768 };
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const fail = message => { throw new Error(message); };
const text = value => typeof value === 'string' && value.isWellFormed() && !value.includes('\0');
function limit(value, key) {
    const n = value === undefined ? DEFAULTS[key] : value;
    if (!Number.isSafeInteger(n) || n < 1 || n > CAPS[key]) fail(`${key} must be an integer from 1 to ${CAPS[key]}.`);
    return n;
}
async function trustedDirectory(path) {
    if (typeof path !== 'string' || !isAbsolute(path)) fail('mailboxDir must be an explicit absolute existing directory.');
    const resolved = resolve(path), stat = await lstat(resolved);
    if (!stat.isDirectory() || stat.isSymbolicLink() || await realpath(resolved) !== resolved) fail('Mailbox directory must not contain symlink aliases.');
    return resolved;
}
async function readBounded(path, maximum) {
    const file = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
    try {
        const stat = await file.stat();
        if (!stat.isFile() || stat.size > maximum) fail('Mailbox file is not regular or exceeds its byte limit.');
        const buffer = Buffer.alloc(maximum + 1);
        let size = 0;
        while (size <= maximum) {
            const { bytesRead } = await file.read(buffer, size, maximum + 1 - size, size);
            if (!bytesRead) break;
            size += bytesRead;
        }
        if (size > maximum) fail('Mailbox file grew beyond its byte limit.');
        return new TextDecoder('utf-8', { fatal: true }).decode(buffer.subarray(0, size));
    } finally { await file.close(); }
}
async function publish(directory, name, body) {
    const final = join(directory, name), lockPath = join(directory, `.publish-${name}.lock`);
    const lock = await open(lockPath, 'wx', 0o600);
    const temporary = join(directory, `.${name}.${randomUUID()}.tmp`);
    try {
        try { await lstat(final); fail('Mailbox output already exists; duplicate publication refused.'); }
        catch (error) { if (error.code !== 'ENOENT') throw error; }
        await writeFile(temporary, body, { flag: 'wx', mode: 0o600 });
        await chmod(temporary, 0o400);
        await rename(temporary, final);
    } finally {
        await lock.close();
        await unlink(temporary).catch(error => { if (error.code !== 'ENOENT') throw error; });
        await unlink(lockPath);
    }
    return final;
}
function decodeReply(raw, id) {
    const reply = JSON.parse(raw);
    if (!reply || Object.keys(reply).sort().join(',') !== 'content,id' || reply.id !== id || !text(reply.content)) fail('Reply must contain the matching id and a well-formed content string only.');
    return reply.content;
}

// Copied as src/models/codex_mailbox.js: Mindcraft discovers static prefixes.
// Construction is inert because upstream also constructs an unconfigured embedder.
export class CodexMailbox {
    static prefix = 'codex-mailbox';
    #model; #params; #calls = 0; #signature; #trace = `.trace-${randomUUID()}.jsonl`; #ready; #queue = Promise.resolve();
    constructor(model, _url, params) { this.#model = model; this.#params = params; }
    async embed() { fail('Codex mailbox embeddings are disabled; no provider was contacted.'); }
    #config() {
        const p = this.#params ?? {};
        if (typeof p !== 'object' || Array.isArray(p)) fail('Mailbox params must be an object.');
        const actor = p.actor ?? this.#model;
        if (typeof actor !== 'string' || !/^[A-Za-z0-9_]{1,16}$/.test(actor) || (p.actor !== undefined && this.#model !== undefined && p.actor !== this.#model)) fail('Mailbox actor must match the Minecraft model name (1–16 letters, digits or underscores).');
        const config = { mailboxDir: p.mailboxDir, actor };
        for (const key of Object.keys(DEFAULTS)) config[key] = limit(p[key], key);
        const signature = JSON.stringify(config);
        if (this.#signature && this.#signature !== signature) fail('Mailbox configuration changed; restart the actor for the new phase.');
        this.#signature = signature;
        return config;
    }
    #record(directory, event) {
        this.#ready ??= writeFile(join(directory, this.#trace), '', { flag: 'wx', mode: 0o600 });
        this.#queue = this.#queue.then(async () => {
            await this.#ready;
            const file = await open(join(directory, this.#trace), constants.O_WRONLY | constants.O_APPEND | constants.O_NOFOLLOW | constants.O_NONBLOCK);
            try {
                if (!(await file.stat()).isFile()) fail('Mailbox trace must remain a regular file.');
                await file.writeFile(`${JSON.stringify(event)}\n`);
            } finally { await file.close(); }
        });
        return this.#queue;
    }
    async sendRequest(turns, systemMessage) {
        const attempt = ++this.#calls, started = performance.now(), createdAt = Date.now(), id = randomUUID();
        const config = this.#config();
        let encoded, requestBytes = 0, validationError;
        try {
            if (attempt > config.maxRequests) fail('Mailbox request quota exhausted; stop this phase.');
            if (!text(systemMessage) || !Array.isArray(turns) || turns.length > 4096) fail('Mailbox requires a well-formed system string and at most 4096 turns.');
            let size = Buffer.byteLength(systemMessage);
            for (const turn of turns) {
                if (!turn || Object.keys(turn).sort().join(',') !== 'content,role' || !['system', 'user', 'assistant'].includes(turn.role) || !text(turn.content)) fail('Each turn must contain only role and well-formed content.');
                size += Buffer.byteLength(turn.content);
            }
            if (size > config.maxRequestBytes) fail('Mailbox prompt exceeds the request byte limit.');
            const packet = { id, actor: config.actor, createdAt, expiresAt: createdAt + config.requestTimeoutMs, maxResponseBytes: config.maxResponseBytes, systemMessage, turns };
            encoded = JSON.stringify(packet); requestBytes = Buffer.byteLength(encoded);
            if (requestBytes > config.maxRequestBytes) fail('Encoded mailbox request exceeds the byte limit.');
        } catch (error) { validationError = error; }
        // One quota refusal is logged; repeated calls cannot grow the trace forever.
        if (attempt > config.maxRequests + 1) throw validationError;
        const directory = await trustedDirectory(config.mailboxDir);
        let responseBytes = 0;
        const event = status => ({ id, actor: config.actor, attempt, createdAt, status, requestBytes, responseBytes, durationMs: performance.now() - started });
        try {
            if (validationError) throw validationError;
            await this.#record(directory, event('requested'));
            await publish(directory, `${id}.request.json`, encoded);
            while (performance.now() - started < config.requestTimeoutMs) {
                let raw;
                try { raw = await readBounded(join(directory, `${id}.response.json`), config.maxResponseBytes); }
                catch (error) { if (error.code !== 'ENOENT') throw error; }
                if (raw !== undefined) {
                    responseBytes = Buffer.byteLength(raw);
                    const content = decodeReply(raw, id);
                    await this.#record(directory, event('answered'));
                    return content;
                }
                await new Promise(resolve => setTimeout(resolve, Math.min(25, config.requestTimeoutMs)));
            }
            fail('Mailbox response timed out; stop the phase without inventing a response.');
        } catch (error) {
            await this.#record(directory, { ...event(validationError ? 'rejected' : 'failed'), error: error.code ?? error.name }).catch(() => {});
            throw error;
        }
    }
}

export async function respond(requestPath, responseTextPath) {
    if (!isAbsolute(requestPath) || !isAbsolute(responseTextPath)) fail('respond requires absolute request and response-text paths.');
    const directory = await trustedDirectory(dirname(requestPath));
    const request = JSON.parse(await readBounded(requestPath, CAPS.maxRequestBytes));
    if (!request || !UUID.test(request.id) || basename(requestPath) !== `${request.id}.request.json` || !/^[A-Za-z0-9_]{1,16}$/.test(request.actor)) fail('Request identity does not match its mailbox filename.');
    if (!Number.isSafeInteger(request.expiresAt) || request.expiresAt <= Date.now()) fail('Request expired; do not answer a stopped phase.');
    const maximum = limit(request.maxResponseBytes, 'maxResponseBytes');
    const content = await readBounded(responseTextPath, maximum);
    if (!text(content)) fail('Reply text must be well-formed UTF-8 without NUL characters.');
    const encoded = JSON.stringify({ id: request.id, content });
    if (Buffer.byteLength(encoded) > maximum) fail('Encoded reply exceeds this request’s response byte limit.');
    if (request.expiresAt <= Date.now()) fail('Request expired while preparing its reply.');
    const responsePath = await publish(directory, `${request.id}.response.json`, encoded);
    return { id: request.id, actor: request.actor, status: 'responded', responsePath };
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
    try {
        if (process.argv.length !== 5 || process.argv[2] !== 'respond') fail('Usage: node codex-mailbox.mjs respond /absolute/UUID.request.json /absolute/answer.txt');
        console.log(JSON.stringify(await respond(process.argv[3], process.argv[4])));
    } catch (error) { console.error(error.message); process.exitCode = 1; }
}

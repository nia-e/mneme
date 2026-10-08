import { open } from 'node:fs/promises';
import { isAbsolute } from 'node:path';
import { performance } from 'node:perf_hooks';

export const LIMITS = Object.freeze({
  queryBytes: 1024, summaryBytes: 512, evidenceBytes: 4096, bodyBytes: 8192,
  actorBytes: 128, requestBytes: 16384, responseBytes: 65536,
  maxCalls: 64, maxTotalBytes: 1048576, timeoutMs: 15000,
});
const PROTOCOL = '2025-11-25';
const ULID = /^[0-7][0-9A-HJKMNP-TV-Z]{25}$/;

export class MnemeError extends Error {
  constructor(code, message, detail = {}) {
    super(message);
    this.name = 'MnemeError';
    this.code = code;
    Object.assign(this, detail);
  }
}

function fail(code, message) { throw new MnemeError(code, message); }
function boundedText(value, field, max) {
  if (typeof value !== 'string' || !value.trim() || Buffer.byteLength(value) > max ||
      value.includes('\0') || !value.isWellFormed()) {
    fail('INVALID_ARGUMENT', `${field} must be nonempty well-formed text of at most ${max} UTF-8 bytes`);
  }
  return value;
}
function id(value) {
  if (typeof value !== 'string' || !ULID.test(value)) fail('INVALID_ARGUMENT', 'id must be a canonical uppercase ULID');
  return value;
}
function limit(value, name) {
  const result = value === undefined ? LIMITS[name] : value;
  if (!Number.isSafeInteger(result) || result < 1 || result > LIMITS[name]) {
    fail('INVALID_CONFIG', `${name} must be an integer from 1 to ${LIMITS[name]}`);
  }
  return result;
}
function endpoint(value) {
  if (typeof value !== 'string' || value.length > 512) fail('INVALID_CONFIG', 'url must be an explicit loopback HTTP endpoint');
  let url;
  try { url = new URL(value); } catch { fail('INVALID_CONFIG', 'url must be an explicit loopback HTTP endpoint'); }
  if (!['http:', 'https:'].includes(url.protocol) ||
      !(url.hostname === '[::1]' || /^127(?:\.\d{1,3}){3}$/.test(url.hostname)) ||
      url.username || url.password || url.search || url.hash || value.includes('?') || value.includes('#')) {
    fail('INVALID_CONFIG', 'url must use numeric loopback HTTP(S), without credentials, query or fragment');
  }
  return url.href;
}
function parseJson(text) {
  // Bound nesting before JSON.parse and before processing the decoded MCP text.
  let depth = 0, quoted = false, escaped = false;
  for (const c of text) {
    if (quoted) {
      if (escaped) escaped = false;
      else if (c === '\\') escaped = true;
      else if (c === '"') quoted = false;
    } else if (c === '"') quoted = true;
    else if (c === '{' || c === '[') { if (++depth > 32) fail('PROTOCOL', 'response nesting exceeds 32'); }
    else if (c === '}' || c === ']') depth--;
  }
  try { return JSON.parse(text); } catch { fail('PROTOCOL', 'response is not valid JSON'); }
}

/** One finite, explicitly configured client. Construct a new instance to reconnect.
 * Body-byte accounting includes initialize/notify/delete and failed responses;
 * HTTP headers/TLS overhead are excluded. No call is automatically retried.
 */
export class MnemeClient {
  #url; #db; #actor; #tracePath; #limits; #trace;
  #seq = 0; #rpc = 0; #calls = 0; #bytes = 0;
  #session; #busy = false; #closed = false; #invalid = false;

  constructor({ url, db, actor, tracePath, maxCalls, maxTotalBytes, timeoutMs } = {}) {
    this.#url = endpoint(url);
    if (typeof db !== 'string' || !/^[A-Za-z][A-Za-z0-9_-]{0,63}$/.test(db)) fail('INVALID_CONFIG', 'db must be an explicit registered database name (1–64 ASCII characters)');
    this.#db = db;
    this.#actor = boundedText(actor, 'actor', LIMITS.actorBytes);
    if (typeof tracePath !== 'string' || !isAbsolute(tracePath) || tracePath.length > 4096 || tracePath.includes('\0')) fail('INVALID_CONFIG', 'tracePath must be an explicit absolute new file path');
    this.#tracePath = tracePath;
    this.#limits = { maxCalls: limit(maxCalls, 'maxCalls'), maxTotalBytes: limit(maxTotalBytes, 'maxTotalBytes'), timeoutMs: limit(timeoutMs, 'timeoutMs') };
  }

  recall(text) {
    return this.#tool('recall_context', () => ({ text: boundedText(text, 'query', LIMITS.queryBytes), k: 8, max_nodes: 32, depth: 2, tags: ['mindcraft'] }));
  }
  get(nodeId) {
    return this.#tool('get', () => ({ id: id(nodeId), body: true, edges: true, max_body_bytes: LIMITS.bodyBytes }));
  }
  remember(summary, evidence) {
    return this.#tool('ingest', () => {
      boundedText(summary, 'summary', LIMITS.summaryBytes);
      boundedText(evidence, 'evidence', LIMITS.evidenceBytes);
      const body = JSON.stringify({ kind: 'mindcraft-observation', observer: this.#actor, evidence });
      boundedText(body, 'body', LIMITS.bodyBytes);
      return { summary, body, tags: ['mindcraft'] };
    }, true);
  }
  supersede(winner, loser) {
    return this.#tool('supersede', () => {
      id(winner); id(loser);
      if (winner === loser) fail('INVALID_ARGUMENT', 'winner and loser must be different explicit IDs');
      return { winner, loser };
    }, true);
  }

  // Explicit operator-only lifecycle action. Never registered as a game command.
  // Success leaves the selected database fenced; this client has no resume API.
  releaseDatabase() {
    return this.#tool('database_control', () => ({ action: 'release' }), true);
  }

  async #record(value) {
    try {
      this.#trace ??= await open(this.#tracePath, 'wx', 0o600);
      await this.#trace.writeFile(`${JSON.stringify({ seq: ++this.#seq, actor: this.#actor, db: this.#db, ...value })}\n`);
    } catch {
      fail('TRACE_IO', 'cannot create or write the exclusive trace file; use a writable new absolute tracePath');
    }
  }

  async #tool(name, argumentsFactory, mutation = false) {
    if (this.#closed) fail('CLOSED', 'client is closed; create a new client to reconnect');
    if (this.#invalid) fail('SESSION_INVALID', 'session is invalid; create a new client and inspect stored state before repeating a write');
    if (this.#busy) fail('BUSY', 'one operation per client may be in flight; await the current call');
    if (this.#calls >= this.#limits.maxCalls) fail('CALL_BUDGET', 'client tool-attempt budget exhausted; stop this phase');
    this.#calls++;
    this.#busy = true;
    try {
      let args;
      try { args = { db: this.#db, ...argumentsFactory() }; }
      catch (error) {
        await this.#record({ event: 'rejected', operation: name, attempt: this.#calls, error: { code: error.code, message: error.message } });
        throw error;
      }
      if (!this.#session) await this.#initialize();
      const response = await this.#exchange({ jsonrpc: '2.0', id: ++this.#rpc, method: 'tools/call', params: { name, arguments: args } }, { mutation });
      return response.payload;
    } finally { this.#busy = false; }
  }

  async #initialize() {
    const response = await this.#exchange({ jsonrpc: '2.0', id: ++this.#rpc, method: 'initialize', params: {
      protocolVersion: PROTOCOL, capabilities: {}, clientInfo: { name: 'mindcraft-mneme', version: '1' },
    } });
    if (response.payload?.protocolVersion !== PROTOCOL || typeof response.session !== 'string' ||
        !/^[\x21-\x7e]{1,256}$/.test(response.session)) {
      this.#invalid = true;
      fail('PROTOCOL', 'initialize did not return the expected protocol and bounded session');
    }
    this.#session = response.session;
    try { await this.#exchange({ jsonrpc: '2.0', method: 'notifications/initialized' }, { notification: true }); }
    catch (error) { this.#invalid = true; throw error; }
  }

  async #exchange(request, { method = 'POST', notification = false, mutation = false } = {}) {
    const body = request === null ? '' : JSON.stringify(request);
    const requestBytes = Buffer.byteLength(body);
    if (requestBytes > LIMITS.requestBytes) fail('REQUEST_BOUND', 'encoded request exceeds 16 KiB');
    // Reserve a whole response frame before doing work; never spend the last few
    // bytes sending a write whose ordinary acknowledgment cannot fit the budget.
    if (this.#bytes + requestBytes + LIMITS.responseBytes > this.#limits.maxTotalBytes) {
      fail('BYTE_BUDGET', 'insufficient cumulative body-byte budget for a bounded response; stop this phase');
    }
    const started = performance.now();
    const operation = request?.params?.name ?? request?.method ?? 'close';
    const seq = this.#seq + 1;
    await this.#record({ event: 'attempt', operation, attempt: this.#calls, request, requestBytes, totalBytes: this.#bytes });
    const abort = new AbortController();
    const timer = setTimeout(() => abort.abort(), this.#limits.timeoutMs);
    let sent = false, status, responseBytes = 0, responseText = '';
    try {
      const headers = { Accept: 'application/json, text/event-stream', 'Content-Type': 'application/json' };
      if (this.#session) { headers['Mcp-Session-Id'] = this.#session; headers['Mcp-Protocol-Version'] = PROTOCOL; }
      sent = true;
      this.#bytes += requestBytes;
      const response = await fetch(this.#url, { method, headers, body: body || undefined, redirect: 'manual', signal: abort.signal });
      status = response.status;
      const length = response.headers.get('content-length');
      if (length !== null && (!/^\d+$/.test(length) || Number(length) > LIMITS.responseBytes)) {
        abort.abort(); fail('RESPONSE_BOUND', 'response content-length exceeds 64 KiB or is invalid');
      }
      const chunks = [];
      if (response.body) {
        const reader = response.body.getReader();
        while (true) {
          const { value, done } = await reader.read();
          if (done) break;
          responseBytes += value.byteLength;
          this.#bytes += value.byteLength;
          if (responseBytes > LIMITS.responseBytes || this.#bytes > this.#limits.maxTotalBytes) {
            abort.abort(); fail('RESPONSE_BOUND', 'response exceeded the reserved 64 KiB bound; stream canceled');
          }
          chunks.push(value);
        }
      }
      try { responseText = new TextDecoder('utf-8', { fatal: true }).decode(Buffer.concat(chunks)); }
      catch { fail('PROTOCOL', 'response is not valid UTF-8'); }
      if (status === 404 && this.#session) {
        this.#invalid = true;
        fail('SESSION_INVALID', 'unknown or expired session; create a new client and inspect stored state before repeating a write');
      }
      if (!response.ok) fail('HTTP', `HTTP ${status}; inspect the trace before retrying`);
      let payload;
      if (notification || method === 'DELETE') {
        if (responseText || (notification ? status !== 202 : status !== 204)) fail('PROTOCOL', 'unexpected lifecycle response');
        payload = null;
      } else {
        if (!/^application\/json(?:;|$)/i.test(response.headers.get('content-type') ?? '')) fail('PROTOCOL', 'expected one finite application/json response');
        const rpc = parseJson(responseText);
        if (!rpc || rpc.jsonrpc !== '2.0' || rpc.id !== request.id ||
            (Object.hasOwn(rpc, 'result') === Object.hasOwn(rpc, 'error'))) fail('PROTOCOL', 'response JSON-RPC envelope or id mismatch');
        if (rpc.error) fail('RPC', typeof rpc.error.message === 'string' ? rpc.error.message : 'JSON-RPC error');
        payload = rpc.result;
        if (request.method === 'tools/call') {
          if (!Array.isArray(payload?.content) || payload.content.length !== 1 || payload.content[0]?.type !== 'text' ||
              typeof payload.content[0].text !== 'string' || typeof payload.isError !== 'boolean') fail('PROTOCOL', 'unexpected Mneme tool result shape');
          if (payload.isError) fail('TOOL', payload.content[0].text);
          payload = parseJson(payload.content[0].text);
          if (request.params.name === 'database_control' && request.params.arguments.action === 'release' &&
              (payload?.state !== 'maintenance' || payload?.db !== this.#db || typeof payload?.db_id !== 'string' || !ULID.test(payload.db_id) ||
               payload?.authority_rotated !== true || payload?.in_flight !== 0 || payload?.backend_jobs !== 0 ||
               typeof payload?.resolved_path !== 'string' || !isAbsolute(payload.resolved_path))) {
            fail('PROTOCOL', 'release did not return a matching quiescent maintenance-state acknowledgment');
          }
        }
      }
      await this.#record({ event: 'result', requestSeq: seq, operation, status, responseText, payload, requestBytes, responseBytes, totalBytes: this.#bytes, durationMs: performance.now() - started });
      return { payload, session: response.headers.get('mcp-session-id') };
    } catch (cause) {
      const ambiguousWrite = mutation && sent && !(status === 404 && this.#invalid);
      const error = cause instanceof MnemeError ? cause : new MnemeError(abort.signal.aborted ? 'TIMEOUT' : 'TRANSPORT', abort.signal.aborted ? 'request timed out' : 'HTTP transport failed');
      error.operation = operation; error.db = this.#db; error.ambiguousWrite = ambiguousWrite;
      if (ambiguousWrite) error.message += '; write may have applied: inspect stored state before any explicit retry';
      try {
        await this.#record({ event: 'error', requestSeq: seq, operation, status, responseText, requestBytes, responseBytes, totalBytes: this.#bytes, durationMs: performance.now() - started, error: { code: error.code, message: error.message, ambiguousWrite } });
      } catch { /* Keep the original write outcome uncertainty visible. */ }
      throw error;
    } finally { clearTimeout(timer); }
  }

  async close() {
    if (this.#busy) fail('BUSY', 'await the in-flight operation before closing');
    if (this.#closed) return null;
    this.#closed = true;
    this.#busy = true;
    try {
      if (this.#session && !this.#invalid) await this.#exchange(null, { method: 'DELETE' });
      return null;
    } finally {
      this.#session = undefined;
      this.#busy = false;
      if (this.#trace) await this.#trace.close();
    }
  }
}

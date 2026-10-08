#!/usr/bin/env node
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { MnemeClient, MnemeError } from './mcp-client.mjs';

export function parseReleaseArgs(args) {
  const values = {};
  if (args.length !== 6) throw new Error('usage: node release.mjs --url http://127.0.0.1:PORT/ --db NAME --trace /absolute/new-trace.jsonl');
  for (let i = 0; i < args.length; i += 2) {
    const name = args[i];
    if (!['--url', '--db', '--trace'].includes(name) || Object.hasOwn(values, name) ||
        typeof args[i + 1] !== 'string' || !args[i + 1] || args[i + 1].startsWith('--')) {
      throw new Error('supply --url, --db and --trace exactly once, with explicit values');
    }
    values[name] = args[i + 1];
  }
  return { url: values['--url'], db: values['--db'], trace: values['--trace'] };
}

/** Quiesce game commands first. This invokes release once and never resumes. */
export async function releaseDatabase({ url, db, trace }, { createClient = (config) => new MnemeClient(config) } = {}) {
  const client = createClient({ url, db, actor: 'operator', tracePath: trace, maxCalls: 1 });
  let result, failure;
  try { result = await client.releaseDatabase(); }
  catch (error) { failure = error; }
  try { await client.close(); }
  catch (error) {
    if (failure) failure.closeError = error.message;
    else failure = new MnemeError('CLOSE_FAILED', 'database release was confirmed, but HTTP session cleanup failed; inspect the trace', { released: true, ambiguousWrite: false });
  }
  if (failure) throw failure;
  return result;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    const config = parseReleaseArgs(process.argv.slice(2));
    const result = await releaseDatabase(config);
    console.log(JSON.stringify({ status: 'released', db: result.db, db_id: result.db_id,
      state: result.state, trace: config.trace,
      message: 'Database is released and fenced. It is safe to stop a host with no other open databases. Release any other databases before stopping their host.' }));
  } catch (error) {
    console.error(JSON.stringify({ status: 'failed', code: error.code, message: error.message,
      ambiguousWrite: error.ambiguousWrite === true, released: error.released === true, closeError: error.closeError }));
    process.exitCode = 1;
  }
}

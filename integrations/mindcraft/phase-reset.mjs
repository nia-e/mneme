import { constants } from 'node:fs';
import { copyFile, lstat, mkdir, open, readFile, realpath, rename, unlink } from 'node:fs/promises';
import { createHash, randomUUID } from 'node:crypto';
import { isAbsolute, join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const conditions = new Set(['mneme', 'native', 'empty']);
const maxHistoryBytes = 4 * 1024 * 1024;

async function regular(path, absent = false) {
  try {
    const info = await lstat(path);
    if (!info.isFile() || info.isSymbolicLink() || info.nlink !== 1) {
      throw new Error(`Expected a single-link regular history file: ${path}`);
    }
    if (info.size > maxHistoryBytes) throw new Error('History exceeds the 4 MiB reset limit.');
    return info;
  } catch (error) {
    if (absent && error.code === 'ENOENT') return null;
    throw error;
  }
}

/** Offline phase boundary only: stop the complete Mindcraft process tree first. */
export async function resetAgentMemory({ botsRoot, agent, condition, receiptPath }) {
  if (typeof agent !== 'string' || !/^[A-Za-z0-9_]{1,16}$/.test(agent)) throw new Error('Invalid Minecraft agent name.');
  if (!conditions.has(condition)) throw new Error('Condition must be mneme, native or empty.');
  if (typeof botsRoot !== 'string' || !isAbsolute(botsRoot) || typeof receiptPath !== 'string' || !isAbsolute(receiptPath)) {
    throw new Error('botsRoot and receiptPath must be explicit absolute paths.');
  }
  const rootInfo = await lstat(botsRoot);
  if (!rootInfo.isDirectory() || rootInfo.isSymbolicLink()) throw new Error('botsRoot must be a real directory.');
  const root = await realpath(botsRoot);
  const directory = join(root, agent);
  try { await mkdir(directory, { mode: 0o700 }); } catch (error) { if (error.code !== 'EEXIST') throw error; }
  const info = await lstat(directory);
  if (!info.isDirectory() || info.isSymbolicLink()) throw new Error('Agent directory must be a real directory.');
  const path = join(directory, 'memory.json');
  if (resolve(receiptPath) === path) throw new Error('Receipt must not replace memory.json.');
  const present = await regular(path, true);
  const before = present ? await readFile(path) : null;
  let data = null;
  if (before) {
    data = JSON.parse(before.toString('utf8'));
    if (!data || Array.isArray(data) || typeof data !== 'object' || typeof data.memory !== 'string' || !Array.isArray(data.turns)) {
      throw new Error('Refusing a file that is not a Mindcraft history record.');
    }
  }
  const retained = condition === 'native' && data ? data.memory : '';
  const after = Buffer.from(JSON.stringify({ memory: retained, turns: [], self_prompting_state: 0, self_prompt: null, taskStart: null, last_sender: null }, null, 2) + '\n');
  const id = randomUUID();
  const backup = before ? join(directory, `memory.before-reset.${id}.json`) : null;
  const temporary = join(directory, `.memory-reset.${id}.tmp`);
  const receipt = { schema: 'mneme.mindcraft.phase-reset.v1', status: 'prepared', agent, condition, history: path, backup, before_sha256: before && hash(before), after_sha256: hash(after), retained_summary_bytes: Buffer.byteLength(retained), process_stop: 'operator prerequisite; not independently observed', created_at: new Date().toISOString() };
  // Reserve the receipt before modifying history; failures leave an explicit record.
  const journal = await open(receiptPath, 'wx', 0o600);
  let tempCreated = false;
  try {
    await journal.writeFile(JSON.stringify(receipt, null, 2) + '\n');
    if (before) await copyFile(path, backup, constants.COPYFILE_EXCL);
    const staged = await open(temporary, 'wx', 0o600);
    tempCreated = true;
    try { await staged.writeFile(after); } finally { await staged.close(); }
    // Refuse ordinary concurrent changes. This is not a lock against a running bot.
    const now = await regular(path, true);
    if (Boolean(now) !== Boolean(present) || (now && (now.ino !== present.ino || now.dev !== present.dev || hash(await readFile(path)) !== hash(before)))) {
      throw new Error('History changed during reset; stop every bot process and inspect the retained backup.');
    }
    await rename(temporary, path);
    tempCreated = false;
    receipt.status = 'reset';
  } catch (error) {
    receipt.status = 'failed';
    receipt.error = error.message;
    throw error;
  } finally {
    if (tempCreated) await unlink(temporary);
    await journal.truncate(0);
    await journal.write(JSON.stringify(receipt, null, 2) + '\n', 0, 'utf8');
    await journal.close();
  }
  return receipt;
}

export async function main(argv) {
  const options = {};
  const keys = { '--bots-root': 'botsRoot', '--agent': 'agent', '--condition': 'condition', '--receipt': 'receiptPath' };
  for (let i = 0; i < argv.length; i += 2) {
    const key = keys[argv[i]];
    if (!key || options[key] !== undefined || !argv[i + 1]) throw new Error('Usage: phase-reset.mjs --bots-root ABS --agent NAME --condition mneme|native|empty --receipt NEW_ABS_FILE');
    options[key] = argv[i + 1];
  }
  return resetAgentMemory(options);
}
if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main(process.argv.slice(2)).then(result => console.log(JSON.stringify(result))).catch(error => { console.error(error.message); process.exitCode = 1; });
}

# Mneme for Mindcraft

This experimental adapter gives Mindcraft bots shared persistent observations,
promises and corrections. It supports local trials comparing Mneme, native
Mindcraft summaries and no persistent memory. It does not provision Minecraft,
accounts or model providers, and a passing smoke test is not evidence of better play.

## Install

You need a Mneme checkout, Rust, Node/npm, and a working local Mindcraft/Minecraft
setup. Use a **separate, disposable Mindcraft checkout** at the revision recorded
in [upstream.json](upstream.json); the installer checks both revision and source
hashes. Follow that checkout's README for game dependencies and compatible Node
versions. The adapter package declares Node 20+; this does not establish that
every newer Node version works with upstream's native/game dependencies.

From the Mneme repository:

```sh
pilot_root="$(mktemp -d "${TMPDIR:-/tmp}/mneme-mindcraft.XXXXXX")"
upstream_revision="$(node -p "JSON.parse(require('fs').readFileSync('integrations/mindcraft/upstream.json')).revision")"
git clone https://github.com/mindcraft-bots/mindcraft.git "$pilot_root/mindcraft"
git -C "$pilot_root/mindcraft" checkout --detach "$upstream_revision"
MINDCRAFT_CHECKOUT="$pilot_root/mindcraft" npm --prefix integrations/mindcraft test
node integrations/mindcraft/install.mjs --mindcraft "$pilot_root/mindcraft"
node integrations/mindcraft/install.mjs --mindcraft "$pilot_root/mindcraft" --check
```

Do not install while Mindcraft is running. The installer adds the command/client
modules and updates command discovery and summary loading. It keeps originals and
an installation manifest in `.mneme-adapter/`; drift or partial installation
refuses. Use a fresh pinned checkout rather than layering manual patches.

Build and start a private Mneme host:

```sh
cargo install --path crates/mneme-mcp --root "$pilot_root/mneme" \
  --locked --no-default-features --features cozo,fastembed,http
mkdir -p "$pilot_root/episode-001/traces"
"$pilot_root/mneme/bin/mneme-mcp" --http 127.0.0.1:8765 \
  --db "village=$pilot_root/episode-001/village.db" \
  --capability-profile operator
```

Use one host and the same `village` alias for all bots in a condition; choose a
new database path for every episode/condition. First use needs the cached
BGE-base model or a model download. Warm one recall before starting concurrent
bots. Never switch embedders on an existing store.

Keep the endpoint on loopback. `operator` is needed for explicit supersession,
so this host is for trusted local trials, not per-bot isolation. Do not run another
Mneme process against its leased database.

## Configure and use

Copy a working upstream profile for each bot, retaining its provider/model
settings. Add this overlay with the actual absolute episode path:

```json
{
  "name": "Alice",
  "memory_condition": "mneme",
  "mneme": {
    "url": "http://127.0.0.1:8765/",
    "db": "village",
    "traceDir": "/absolute/episode-001/traces",
    "maxCalls": 64,
    "maxTotalBytes": 1048576,
    "timeoutMs": 15000
  }
}
```

Create `traceDir` before launch; each bot writes its own trace. Use valid, distinct
Minecraft names and restart when changing condition, episode or configuration.
Start bots with the ordinary upstream launcher after confirming basic movement,
building and observation in a throwaway world. Keep insecure generated-code
execution disabled.

| Condition | Persistent memory |
| --- | --- |
| `mneme` | Shared Mneme store; native summaries disabled. |
| `native` (also the default when omitted) | Upstream per-agent summaries; adapter commands hidden/denied. |
| `empty` | Neither summaries nor adapter commands. |

Recent within-phase conversation remains available in every condition. Native and
empty do not contact Mneme.

## Four commands

| Command | Use |
| --- | --- |
| `!recallExperience("question")` | Search shared summaries. |
| `!inspectExperience("ID")` | Read evidence and relations for a returned ID. |
| `!rememberExperience("summary", "evidence")` | Save an observation or promise with its observer. |
| `!supersedeExperience("NEW_ID", "OLD_ID")` | Correct known old advice after saving the replacement. |

The pinned parser needs double-quoted strings **without embedded double quotes**;
it has no escaped-quote or JSON argument syntax. UTF-8 limits are 1024 bytes for
a query, 512 for a summary and 4096 for evidence; IDs are canonical 26-character
ULIDs. Include concrete locations/events and scope in observations.

Remember first, then supersede using the acknowledged IDs. Inspect the old record
to verify its replacement relation. Recalls can be partial or stale; check whether
remembered conditions still apply. The adapter does not filter away false advice
or add automatic feedback/core promotion. A write timeout has an uncertain
outcome: inspect traces/store before repeating it. Writes are never replayed
by the client automatically.

## Smoke before the first game

With a populated model cache:

```sh
FASTEMBED_CACHE_DIR=/absolute/populated/fastembed-cache \
  node integrations/mindcraft/smoke.mjs \
  --binary "$pilot_root/mneme/bin/mneme-mcp" \
  --output "$pilot_root/http-smoke-001"
```

The output directory must be new. The script starts/stops a disposable host and
checks three clients, shared observations, corrections, fresh sessions and restart
persistence. It does not exercise models or Minecraft. Missing models or refused
loopback binding are failures, not reasons to substitute lexical embeddings.

Append `--shutdown-signal SIGINT` or `--shutdown-signal SIGTERM`, with a new output
directory, to exercise signal-driven shutdown. Retain the supplied binary's
source/features/hash with the smoke evidence.

## Stop the host cleanly

Stop agent work, then press Ctrl-C or send SIGTERM to a current HTTP host. Wait
for it to drain requests, checkpoint and release registered databases. A failed
or timed-out shutdown needs review; SIGKILL and power loss are not graceful stops.

For offline maintenance or an older host without signal handling:

```sh
node integrations/mindcraft/release.mjs --url http://127.0.0.1:8765/ \
  --db village --trace "$pilot_root/episode-001/release-before-stop.jsonl"
```

Require a successful maintenance acknowledgment before terminating it. Release
every database on a multi-store host; closing an HTTP session alone does not
release a store. Do not delete a write log to resolve a slow checkpoint.

## A small village trial

Use fixed tasks, time budgets and an initial world copy for each condition:

1. **Prepare:** mark a starter house and a tunnel with a repeatable water failure.
   Verify the failure and repair manually; keep coordinates/actions in an
   operator-only log, not agent instructions.
2. **Learn:** give ordinary settlement work, an opportunity to observe the failure,
   and a promise to preserve the house. Record what agents actually discover/save.
3. **Return:** stop all bot processes, retain only the assigned persistent memory,
   reset phase state below, and restart with work that can expose repeated mistakes.
4. **Transfer:** use a fresh bot name without predecessor transcripts. Stop
   predecessors during isolated recall; direct messaging is a separate teaching
   channel. Native memory is per-agent, not a shared newcomer handoff.
5. **Revise:** reveal the repair while keeping the house promise. Observe whether
   agents check conditions, correct obsolete advice and preserve still-valid facts.

After stopping every bot process, reset each returning agent with a new receipt:

```sh
node integrations/mindcraft/phase-reset.mjs \
  --bots-root "$pilot_root/mindcraft/bots" --agent Alice --condition mneme \
  --receipt "$pilot_root/episode-001/reset-Alice-return.json"
```

Set `load_memory: true` in upstream `settings.js` for all returning conditions.
The reset backs up `memory.json`, clears turns/self-prompt/task state, and retains
only native's existing summary. Mneme/empty summaries are cleared. It does not
summarize turns, modify the world/store or prove processes stopped. `!clearChat`
and `load_memory=false` alone are not clean resets.

Keep independent before/after world observations alongside JSONL client traces.
Score repeated costly mistakes, promise violations, transfer, stale warnings and
task completion. Separate pathfinding/game-control failures from memory failures.
Record failed attempts, elapsed time, memory bytes/calls and provider token/cache
usage where available—including native summarization. Missing telemetry is unknown,
not zero. `chat_bot_messages=false` hides echoes but does not disable direct
MindServer delivery; keep other channels quiet for isolated transfer.

A fair quality comparison also needs searchable notes, matched models/budgets and
multiple seeds. This adapter does not implement that comparator; a successful
first game establishes feasibility, not Mneme superiority.

## Optional supervised Codex mailbox

For a supervised local trial, copy the mailbox provider into the pinned checkout:

```sh
cp integrations/mindcraft/codex-mailbox.mjs \
  "$pilot_root/mindcraft/src/models/codex_mailbox.js"
```

Configure a bot profile with an existing absolute mailbox directory:

```json
{
  "model": {
    "api": "codex-mailbox",
    "model": "Alice",
    "params": {"mailboxDir": "/absolute/private/mailbox", "maxRequests": 10}
  },
  "embedding": "codex-mailbox/embeddings-disabled",
  "conversation_examples": [],
  "coding_examples": []
}
```

Set `num_examples: 0` in upstream `settings.js`.

The provider writes immutable `UUID.request.json` conversations without network
calls. It does not dispatch Codex tasks: a supervisor must supply each completion
using a fresh context containing only the actual system prompt/turns, not scoring
notes or predecessor conversations. Publish the decoded answer verbatim:

```sh
node integrations/mindcraft/codex-mailbox.mjs respond \
  /absolute/private/mailbox/UUID.request.json /absolute/private/answer.txt
```

When relaying through a Codex subagent, request exactly `{"content":"..."}` and
decode the string without trimming; retain the raw envelope. Missing/malformed
replies are relay failures, not permission to invent an answer. This preserves
invisible answers such as tabs without relying on task-message formatting.

Requests expire after two minutes by default. The supervisor must enforce the
total phase budget and stop the complete Mindcraft process group on failure;
automatic upstream restarts otherwise create fresh provider quotas. Traces report
calls, bytes, duration and status, not Codex tokens/cache/subscription cost. This
supervised bridge is not an identical substitute for a model API.

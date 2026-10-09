# CLI, MCP and agent workflows

Use `mnemed` for terminal work and scripting, or connect `mneme-mcp` to an agent.
Both use the same memory operations. Start with save, recall and inspect; curation
and maintenance are separate tasks.

<a id="as-a-cli-mnemed"></a>

## Everyday CLI use

Install from the repository root:

```sh
cargo install --path crates/mnemed --locked
cargo install --path crates/mneme-mcp --locked
```

For project setup with Codex, see [setup below](#selective-codex-project-memory).
The following examples assume a configured project owner: the running server
that holds the store open. [Remote CLI](remote-cli.md) explains owner selection
and explicit HTTP/SSH connections.

```sh
mnemed save "Tokio is an async runtime for Rust" \
  --tags rust --body "executor, reactor, timer" --operation-id tokio-note-1
mnemed recall-context "rust async runtime"
mnemed query "rust async runtime" --json
mnemed get NODE_ID --body --edges
mnemed list --tag rust --limit 64
```

| Task | Command |
| --- | --- |
| Get model-ready context | `recall-context "topic"` |
| Inspect raw search results | `query "topic"` |
| Read one memory | `get ID`; `body ID` for body pages |
| Record an event | `save --kind episode "scene"` |
| Browse stored nodes | `list`; `neighbors ID` for links |
| Browse observed tag names | `list --tags` |
| Browse visually | `tui` |
| Review pending work | `status`, then `reconcile` or `merges` |

Use `mnemed COMMAND --help` for arguments and limits. `--json` retains coverage
and continuation metadata; `recall-context` always emits compact JSON.

### Save and retry

SAVE needs an existing current store. It does not initialize or upgrade storage.
Use `save --input request.json` for a complete request with source identity, links,
occurrence fields or touchstone references. See [episodes](episodic-memory.md)
and [touchstones](touchstones.md) for examples.

For safe exact retries, retain a stable `--operation-id` or genuine source identity
**and the complete unchanged request**, including file contents. Reusing an identity
with changed content conflicts. Without either identity, each call is a fresh
manual submission. The CLI displays a generated operation ID before execution;
a raw MCP caller losing the generated response has an ambiguous outcome.
JSON-RPC request IDs are not SAVE replay identities.

Receipts return the canonical `id`, kind, replay status, origin and selected
`db`/`db_id`, but not the body. Manual saves return `operation_id`; episodes also
return root, edition and revision identities. `manual_submission` records deliberate
authoring, not observation evidence. No write is automatically retried after a
transport failure.

### Choose a store

Ordinary commands use the configured project owner, or permitted device misc
when project configuration is genuinely absent. `--user` selects the separately
configured private user owner. `--remote URL` chooses an explicit connection.
Failure never causes a fallback to another store.

Explicit `--db PATH` or `MNEME_DB` selects offline storage instead. The store must
be current and not held by another process. Use the
[maintenance handoff](maintenance-handoff.md) before opening a live server's store
for offline work. Provisioning, upgrades and index replacement require explicit
`--db`; `--remote-db NAME` is a server registry name, not a filesystem path.

## Standalone use without Codex

With the default-feature build installed above, initialize an absent store, then
use its explicit path for each command. This requires no Codex executable, hooks
or background server:

```sh
mkdir -p .mneme
mnemed --db "$PWD/.mneme/memory.db" capture init
mnemed --db "$PWD/.mneme/memory.db" save "The parser accepts UTF-8 input" \
  --tags parser --operation-id parser-note-1
mnemed --db "$PWD/.mneme/memory.db" recall-context "parser input"
```

Do not run initialization against an existing store. Once a server owns this
path, use its connection instead of these offline commands.

To expose the store to an MCP client, configure its stdio server launcher with
an absolute executable path and the same absolute database path. For clients
using an `mcpServers` JSON object:

```json
{
  "mcpServers": {
    "mneme": {
      "command": "/absolute/path/to/mneme-mcp",
      "args": ["--capability-profile", "curator", "--db", "project=/absolute/path/to/memory.db"]
    }
  }
}
```

Choose `read-only` instead of `curator` if the client should only browse. Other
clients use their own configuration syntax with the same command and arguments.
Only one server may own the database at a time.

## Browse, inspect and deliberately learn

`stores [--json]` lists configured owner/library metadata without contacting
servers or opening stores. An entry is configuration, not proof of reachability.
`--user`, `--remote URL` and `stores --config PATH` restrict that inventory.
It does not support `--db`/`MNEME_DB`.

`mnemed --json get ID` and MCP `get` inspect one exact record. Current owners
return the complete canonical summary, up to 16 KiB of UTF-8; MCP reports
`summary_truncated: false`. JSON results also include `content_fingerprint` and
`content_fingerprint_codec` for [guarded tag edits](#guard-content-used-to-choose-tags).
Older owners may still truncate summaries: inspect `summary_truncated` before
treating a response as complete. This does not widen presentation cards: MCP
`core` and `query` summaries remain capped at 2 KiB, node inventory at 1 KiB, and
recall-context output at its own budget. Body ranges remain separate and bounded.

`list [--status active|archived|all] [--tag TAG] [--limit N] [--after CURSOR]`
returns nodes in canonical ID order, not relevance or newest-first order.
It defaults to all statuses and 50 cards, with a maximum of 64. Historical episode
editions are included. Follow `next_cursor` with unchanged filters, even when a
filtered page is empty. Cursors fix the initial upper key but do not provide an
atomic snapshot. [Touchstones](touchstones.md#find-and-browse) have a separate
indexed collection.

`neighbors ID --limit 64 --after CURSOR` pages incident edges in both directions,
including missing and episode endpoints. It is topology inspection, not a list
of legal walk moves. Preserve its completeness and continuation metadata.
The [Observatory](observatory.md) uses separate read-only graph pages and lazy
summary batches, without loading bodies or running inference for the inventory.

`repl ID --budget N` starts a bounded walk. Bare `done`, `abort` and EOF finish
without training. `done USED_ID…` explicitly reflects only visited nodes that
contributed, subject to receipt-grounded authority. A lost reflection response is
not permission to retry by creating a new walk.

Cross-store links use `link --to-remote-db NAME` for an owner registry entry or
`--to-db PATH` for offline storage. Only user memory can originate cross-store
links. Snapshot publication is an operator workflow; see
[memory libraries](memory-libraries.md).

## Selective Codex project memory

```sh
mnemed init                         # project setup, including automatic recording
mnemed init --no-recording          # recall and inspection without recording
mnemed init --root /path/to/project
```

`init` creates or adopts `.mneme/codex-memory.db`, starts its shared owner and
writes CLI, MCP and hook configuration. It registers the verified owner locally,
not for snapshots, peers or hosted tunnels. Existing off/private/isolated choices
and conflicting stores are not silently replaced.

Setup requires Python 3.11+, Codex and a matching `mneme-mcp`. Override discovery
with `--python`, `--codex-binary` or `--mcp-binary`. `--json` reports completed stages
and recovery actions. Hook trust is limited to matching installed Mneme hooks;
`--no-trust-hooks` leaves trust unchanged. If Codex's configuration API is unavailable
or the installed definitions differ, trust remains pending for manual review.

The [Codex integration guide](../integrations/codex/README.md) covers scope,
recording, hook review and recovery. Optional `.mneme/hippocampus.md` provides
project capture guidance; it is not foreground memory or authority to widen scope.
Device misc recording requires its own reviewed configuration.

### Initial knowledge for an existing project

`init` does not read the repository or seed an orientation graph. After setup,
ask an agent to use
[`mneme-seed-project`](../.agents/skills/mneme-seed-project/SKILL.md):

> Seed this project's memory from the current source: purpose, component
> boundaries, important invariants and build/test workflows.

The skill checks source and existing memory, then saves missing orientation with
source identity and exact readback. It does not automatically mark notes core.
Managed repository bootstrap is a separate [workflow](#agent-workflows).

## As an MCP connector — recommended for agents / chat

Start one server per set of stores and connect your client to it. The server holds
an exclusive lease on each store; a second server or offline CLI opening the same
path fails. CLI owner connections share the existing server.

The server defaults to `receipt-grounded`. Select a profile explicitly when it
needs different authority:

| Profile | Available operations |
| --- | --- |
| `read-only` | Retrieval, topology/body inspection, status and read-only walks |
| `receipt-grounded` | Read-only operations plus `reflect` with a server-issued walk receipt |
| `curator` | Adds non-core saves, episode appends, non-core tag edits, links and contradiction flagging |
| `operator` | Adds core curation, note-body/summary editing, forgetting, adjudication, maintenance and lease control |

Profiles are fixed at startup and enforced in both the tool catalog and calls.
They are authority limits, not authentication or isolation from another process
running as the same OS user. Direct ungrounded feedback is disabled unless the
operator explicitly enables the deprecated compatibility flag.

### Connect locally

For a Claude Desktop extension, build the
[`.mcpb` bundle](../packaging/mcpb/build.sh); it includes the embedding model for
offline use. For stdio or local Streamable HTTP, select existing current stores:

```sh
mneme-mcp --capability-profile curator --db project=/absolute/memory.db
mneme-mcp --capability-profile read-only --http 127.0.0.1:8765 \
  --db project=/absolute/memory.db
```

HTTP initialization is a single JSON-RPC request without a session header.
Retain the returned `mcp-session-id`. Later POSTs include that ID, the negotiated
`mcp-protocol-version`, `Content-Type: application/json`, and an `Accept` value
containing both `application/json` and `text/event-stream`. The server supports
MCP `2025-11-25` and supported older revisions. Batches are rejected; DELETE closes
a session; server-initiated GET/SSE is not implemented.

Non-loopback HTTP requires a bearer token named by `--http-token-env VAR`.
Put TLS in front and allow a browser origin only when needed with
`--cors-origin https://exact.example`. The token grants every operation in the
selected profile; this is not a multi-tenant service or per-call approval system.
See [hosted connections and multi-owner routing](remote-cli.md#chatgpt-and-hosted-clients).

### Database routing and limits

Tools select a logical `db` from the server registry. Reads without `db` use
`project`, or the sole entry if there is no project. Other ambiguous selections
fail. Every mutation requires explicit `db`. An unavailable project never falls
back to user. `expected_db_id` guards retained identities; configured CLI owner
calls require guarded reads as well as writes.

MCP requests cap at 2 MiB. SAVE note bodies cap at 256 KiB and episode bodies at
16 KiB. Tool text caps at 256 KiB decoded; complete serialized JSON-RPC responses
cap at 512 KiB on both transports, including stdio framing. Explicit body ranges
default to 64 KiB and cap at 1 MiB, but the response ceiling can refuse a large
range. Follow `next_offset` and `has_more` rather than assuming a prefix is complete.

All registered databases share one serial inference runtime. It admits one
executing model request and three queued requests, with a five-second queue
deadline. HTTP rejects excess work above 32 in-flight POST requests. Per-call
limits do not impose an aggregate cross-call task budget.

For offline maintenance, operator `database_control` can release and resume a
store's lease after work and outstanding receipts drain. See the
[maintenance handoff](maintenance-handoff.md); do not start a competing writer.

## Tags are reusable filters

Use broad, stable tags such as `rust`, `filesystem` or `pitfall`. Put claim-specific
distinctions in prose and research-run identity in provenance. Reuse established
filters; tags match exactly and case-sensitively. No synonym expansion or automatic
normalization occurs.

Inventory pages and `get` show existing tags. Query hits are only a partial view;
graph pages may mark tag prefixes `tags_truncated`. A truncated set is unknown,
not proof that a tag is absent. A full-store scan is not required before each save.

For people, use `people`, an established exact handle/name when they are a
substantial subject, and useful role/team filters. Dates and qualified membership
claims belong in prose. There is no special person node or mandatory tag taxonomy.

### Browse observed vocabulary

`list --tags` browses distinct indexed tag names, not nodes with a matching tag.
MCP uses `list` with `kind: "tags"`. It covers semantic notes only, excluding
episode tags; the result is observed vocabulary, not a canonical taxonomy.

```sh
mnemed --json list --tags --prefix rust --status active --limit 32
```

```text
list {db:"project", expected_db_id:"<verified database ID>",
      kind:"tags", prefix:"rust", status:"active", limit:32}
```

The prefix is optional, exact and case-sensitive; an empty prefix selects all
names. Status is `active`, `archived` or `all` (the default). Pages contain up to
50 names by default, with a maximum of 64, in ascending tag order. Each item has
`name`, `count` and `examples` (up to three node IDs). Examples are bounded samples,
not ranked representatives. Interpret `count.status` explicitly:

- `exact`: `value` is the observed membership count for the selected status.
- `lower_bound`: `value` is a known minimum, not a total.
- `unavailable`: no count is supplied; this does not mean zero.

Pass `next_cursor` unchanged as CLI `--after` or MCP `after`, keeping the same
database, prefix and status. Continue until the cursor is null, even when a
filtered page is empty. Cursors bind that selection and database identity, not a
snapshot; concurrent changes may require a fresh pass.

`has_more` describes continuation. `partial` is also true when any returned count
is not exact, so it can remain true with no next cursor. `coverage.counts_complete`
says whether all counts on this page are exact. Each call permits at most
256 indexed name/status seeks and 4,096 membership rows, with a 128 KiB response allowance;
`coverage` reports the work and stopping reason. No full-node scan, body loading,
inference or learning is performed. `--tags` cannot be combined with the node
filter `--tag` or with `--touchstones`.

### Open possibilities are ordinary notes

Use `possibility` for an open idea or question. State the question in the summary,
since some context views omit tags. Add `pursuing` only after choosing to act.
When finished or declined, remove `pursuing` and add `closed`; do not combine them.
Keep the original thought and link an outcome where useful.

```sh
mnemed save "Open question: could cluster labels explain their grouping?" \
  --tags possibility,tui --operation-id cluster-label-question-1
mnemed list --tag possibility --limit 64
mnemed query "explaining graph clusters" --tag possibility --depth 0
```

These tags are conventions, not a task state machine or execution authority.
Closed possibilities remain searchable unless separately archived. `list` includes
closed notes and pages an inventory; `query` ranks matches. Depth zero avoids
expansion beyond the tagged seeds.

## Edit tags in place

`retag` replaces a semantic note's **complete tag set** if it matches the complete
`expected_tags` you inspected. Order does not matter; duplicates are invalid.
Retain unrelated tags:

```sh
mnemed get NODE_ID
mnemed retag NODE_ID --expected-tags possibility,tui --tags possibility,closed,tui
mnemed retag NODE_ID --expected-tags possibility --tags  # empty replacement
```

```text
retag {db:"project", expected_db_id:"<verified database ID>",
       id:"<node ID>", expected_tags:["possibility","tui"],
       tags:["possibility","closed","tui"]}
```

Curator can edit non-core sets; either set containing `core` requires operator.
The reply returns `id`, canonical tags, `changed` and `db`/`db_id`. Other note content
and state stay unchanged. Episode editions and authored touchstone restrictions
still apply. Replaying original SAVE does not reset later tag edits.

Configured owners supply the database guard; explicit `--remote` needs
`--expected-db-id`. Stale tags refuse: read again before deciding on a new edit.
This checks current values, not monotonic history; a change away and back can make
an old expected set match again. Lost acknowledgements are ambiguous, not permission
to replay blindly. Owners lacking checked retag and read-only copies refuse without
fallback.

### Guard content used to choose tags

If a tag decision depends on a note's meaning or a policy guide, keep the
`content_fingerprint` from each full GET and add the optional content guards:

```sh
mnemed --json get NODE_ID
mnemed --json get GUIDE_ID
mnemed retag NODE_ID --expected-tags rust --tags rust,concurrency \
  --expected-content-fingerprint TARGET_FINGERPRINT \
  --guard-node GUIDE_ID=GUIDE_FINGERPRINT
```

```text
retag {db:"project", expected_db_id:"<verified database ID>",
       id:"<node ID>", expected_tags:["rust"], tags:["rust","concurrency"],
       expected_content_fingerprint:"<target content_fingerprint>",
       guard_nodes:[{id:"<guide ID>", content_fingerprint:"<guide content_fingerprint>"}]}
```

The backend checks expected tags, target fingerprint and all guard nodes in the
same write snapshot. Stale fingerprints or missing guard nodes refuse even a
no-op; a separate GET preflight does not replace this atomic check. Guard nodes must be
distinct semantic notes in the same database, with a maximum of 1,024; archived
notes are allowed. Repeat `--guard-node` for additional guards. `guard_nodes`
requires `expected_content_fingerprint`, but the target guard can be used alone.

Fingerprints are opaque lowercase SHA-256 values with a reported codec. They bind
canonical content, including tags and the body reference, not mutable external
body bytes, lifecycle status or learning counters. Use the complete canonical
summary as guide text, not body bytes. These are current-content guards, not a
monotonic edit history or proof that an ambiguously acknowledged write committed.

Omitting the new fields retains tag-only compare-and-replace. Native clients check
the owner's advertised support before sending strong guards; an older owner may
support tag-only edits but cannot silently receive an unguarded substitute. Profile
requirements and the successful reply shape are unchanged.

## Edit a note body in place

Operator `edit-body` / MCP `edit_body` replaces only an ordinary semantic note's
body. Inspect the content, follow any body continuations, and keep the opaque
`body_revision` from `get`:

```sh
mnemed --json get NODE_ID
mnemed body NODE_ID
mnemed edit-body NODE_ID --expected-body-revision REVISION --body-file revised.md
```

```text
edit_body {db:"project", expected_db_id:"<verified database ID>",
           id:"<node ID>", expected_body_revision:"<body_revision from get>",
           body:"<complete replacement>"}
```

The replacement is UTF-8, at most 256 KiB; empty is allowed. ID, summary/search
embedding, tags, original provenance, links and learning state stay unchanged.
The reply returns the new revision, not the body. Configured owners supply the
database guard; explicit `--remote` needs `--expected-db-id`.

The revision checks body-reference identity, not a content hash or edit ledger.
Stale revisions refuse; a lost acknowledgement requires inspection. Original SAVE
replay does not undo the edit, and original provenance does not certify new text.

Episode editions, touchstone bodies and notes with outgoing body-span anchors
cannot use this operation. Use episode revisions or new touchstones instead;
anchored bodies require a deliberate reference-editing workflow. Old blobs remain
for shared and in-flight readers; the operation provides neither secure erasure
nor browsable edit history. No schema migration or re-embedding is required.

## Edit a note summary without replacing its identity

Operator `edit-summary` / MCP `edit_summary` replaces an ordinary semantic summary
and its derived search projections atomically. Use it for wording corrections;
a changed conclusion may deserve supersession instead.

Read the full current summary and copy
`summary_snapshot.expected_snapshot_sha256` from GET:

```sh
mnemed --json get NODE_ID
mnemed edit-summary NODE_ID --expected-snapshot-sha256 HASH \
  --summary "The corrected searchable summary"
```

```text
edit_summary {db:"project", expected_db_id:"<verified database ID>",
              id:"<node ID>", expected_snapshot_sha256:"<GET snapshot hash>",
              summary:"The corrected searchable summary"}
```

The replacement is nonblank UTF-8, at most 16 KiB. Episode editions, touchstones,
stale guards and unchanged text refuse; unchanged text does no embedding work.
Embedding or transaction failure preserves old content and search state. The ID,
body, tags, provenance, links, lifecycle and learned state stay intact.

The result returns `id` and `summary_snapshot_sha256`; read back after the edit.
Configured owners supply the database guard; explicit `--remote` needs
`--expected-db-id`. Lost acknowledgements require inspection. The guard checks
current content, not monotonic history: A → B → A can restore an old A guard.
Original SAVE replay does not restore old wording. Historical touchstone snapshots
and concern meaning bindings retain their original content; body-span anchors
are unaffected. Old owners and read-only copies refuse without fallback.

## Explicit initialization

To create storage without project integration, select an absent path in an
existing directory, with no other owner:

```sh
mnemed --db /absolute/existing-directory/memory.db capture init
```

This creates a current store, not an upgrade. Ordinary SAVE and hooks never
initialize missing stores. For predecessors, use the
[detached migration steps](episodic-memory.md#existing-persistent-stores), with
paired database/body backups and a matching runtime.

## Agent workflows

The focused [recall](../.agents/skills/mneme-recall/SKILL.md),
[remember](../.agents/skills/mneme-remember/SKILL.md) and
[reconcile](../.agents/skills/mneme-reconcile/SKILL.md) skills cover those tasks.
[Codex memory](../.agents/skills/mneme-codex/SKILL.md) covers enabled project/device
scope; the [reference skill](../.agents/skills/mneme/SKILL.md) documents shared mechanics.

- Recall with bounded context, then inspect exact evidence when needed.
- Save durable facts, decisions, gotchas or selected episodes with honest identity.
  Link known relations; an association means “consider together,” not agreement.
  Similarity linking and experimental consolidation are disabled by default.
- Reconcile conflicts and merge redundancies deliberately. Decay and prune are
  separate explicit edge-maintenance operations.

### Walk receipts and reflection

Walks are read-only and restrict navigation to legal moves within a node budget.
A completed MCP walk returns an opaque receipt. Reflect only after deciding which
visited nodes contributed; callers cannot supply invented trails. Reads alone
create no reinforcement or co-retrieval learning.

A receipt batch updates its local store atomically. Exact retry is idempotent only
while the issuing MCP host epoch remains alive and exclusively owns the database.
Saving does not clear that proof; restart, reopen or snapshot import invalidates
old receipts. A restart cannot recover a lost pre-restart acknowledgement.
Reflection retains stored edge direction even when a walk followed an incoming
edge, and does not recreate vanished routes.

### Repository bootstrap

[`mneme-bootstrap`](../.agents/skills/mneme-bootstrap/SKILL.md) plans a source-bound
repository graph from a frozen committed tree. Begin with read-only
`mnemed --json bootstrap-inspect --root .` to select the appropriate mode.

Only reviewed absent-store greenfield plans have a native creation path through
`bootstrap-create`. It verifies and publishes a generation under an exclusive
lease; exact retries are operation-scoped. Managed refresh and brownfield adoption
produce proposals, not executable updates: there is no `managed-apply` command.
Sequential SAVE calls are not a substitute for atomic managed refresh.

Inventory follows `.mneme/current` to the selected generation manifest and rejects
malformed/orphan selectors or competing conventional stores. The receipt HMAC key
is stored beside the graph: it checks artifact consistency, not protection against
another process running as the same OS user. Exact retry verifies historical
creation evidence, not the graph's content after subsequent legal mutations.

# Episodic memory

Semantic notes keep what you learned. Episodes keep selected accounts of what
happened: an attempt, discovery, change of mind or shared moment. They share a
graph but have separate search and retention policies.

An episode is not a transcript or task queue. Read it as state at the time.
A later event can change the situation without making the earlier account wrong.
“Current edition” means the latest edit of that account, not verified live state.
Occurrence time and recording time are separate.

## Write one scene

For a short manual account:

```sh
mnemed save --kind episode "The first async prototype worked" \
  --operation-id async-scene-1
```

For source identity, body text, occurrence contexts and other fields, write a
complete SAVE request to `scene.json`:

```json
{
  "kind": "episode",
  "source": {
    "namespace": "experiment",
    "key": "sample-app/retry-test",
    "reference": "journal://sample-app/retry-test"
  },
  "summary": "The retry test reproduced a duplicate write after a lost acknowledgement.",
  "body": "The test committed a write, discarded its acknowledgement and retried with a new operation ID. A second write appeared. Retrying with the original ID returned the first result.",
  "thread": "sample-app",
  "occurrence_contexts": [
    {"namespace": "project", "key": "sample-app", "label": "Sample application"},
    {"namespace": "room", "key": "shared-workbench"}
  ]
}
```

```sh
mnemed save --input scene.json
```

These commands use a configured owner. For an initialized offline store, add
`--db memory.db`; SAVE never creates or upgrades it. Retain the entire unchanged
request and its source or operation identity for retry. Reusing a source key with
changed content conflicts; use a new key for a new event, or revise the account
as described [below](#correct-the-account-preserve-the-original).

Manual saves record `manual_submission`, not session observation evidence.
Without source/operation identity, SAVE generates a fresh manual ID. The CLI
shows it before execution; losing a raw MCP reply with that generated ID leaves
an ambiguous outcome. JSON-RPC IDs are not replay identities. See
[SAVE retry rules](cli-and-mcp.md#save-and-retry).

### Time and grouping

Optional `occurred` uses Unix epoch milliseconds, as a point or interval:

```json
{"kind":"point","at":1790460000000}
```

```json
{"kind":"range","start":1790456400000,"end":1790460000000}
```

Unknown occurrence time stays unknown. Recording time is assigned on the first
successful write; it is not substituted for occurrence. `thread` is an opaque
historical grouping label, not an access boundary.

### Where it happened, not who recorded it

Optional `occurrence_contexts` names where the event happened independently of
its recorder. Omission means unknown; never infer context from source, session,
host or working directory. Semantic notes reject this field.

When provided, the array is nonempty and each `{namespace, key, label?}` has a
unique exact namespace/key pair. Values are case-sensitive, trimmed, nonblank
and control-free. Labels are display metadata, not identity or a shared registry.
The array is canonicalized by namespace/key, so reordering preserves intent.

The whole canonical compact JSON array must fit 1024 UTF-8 bytes, including
escaping and labels. Identifiers are never truncated to fit. Use a client/server
schema supporting the field; wrappers must not silently discard it.

## Find a past event

```sh
mnemed episode list --limit 8
mnemed episode list --axis occurred --thread sample-app
mnemed episode search 'retry test'
mnemed episode get EPISODE_ID --body
mnemed episode history EPISODE_ID
```

List returns current editions, newest recorded first by default. Occurrence
order excludes unknown times; occurrence windows match overlapping intervals,
not just their starts. Cue search is lexical over current summaries, not semantic
search. The [Observatory Scenes view](observatory.md#scenes-when-it-happened-when-it-was-recorded)
provides a visual timeline.

Ordinary `recall-context` combines current lexical episode hits with one-hop exact
references from initial semantic and episode hits. Historical references retain
their editions; finding a newer head is not reading that newer account. New linked
scenes do not recursively discover further scenes.

Episode cards retain root/edition identity, times, occurrence contexts, edition
recording provenance and discovery origins. Origins describe lexical matches or
exact references, not endorsement, causation or learning. Episode cards share the
same context byte budget as semantic cards and are omitted whole when needed.

Check coverage before treating an empty episode section as empty history. Lexical
search coverage, reference-discovery coverage and presentation omissions are
separate. Tagged recall may skip lexical episode search while still finding
scenes through tagged semantic anchors; that does not make them episode tag matches.
Use explicit timelines or focused lexical search when chronology matters.

### Linked-read allowance

Linked discovery is capacity-derived and bounded: requested capacity `N` has an
effective native ceiling of 256, with at most `2N` incident-edge rows scanned and
`N` uncached endpoint reads. Missing, semantic and duplicate endpoints consume
work too. One shared five-second native deadline bounds this linked stage.

Partial failures preserve useful semantic/lexical results and disclose reference
coverage. These bounds do not promise complete discovery or fixed disk-byte work.
A returned page or context window is not a complete history.

### Explicit episode pages and bodies

Pages default to eight items and cap at 32. Follow `next` using `--after` with
unchanged filters and order. An empty partial page may still offer continuation.
Cursors are positions, not snapshots; concurrent edits and backdated events can
affect subsequent pages.

Episode summaries cap at 2 KiB, bodies at 16 KiB and combined encoded metadata at
16 KiB. These are checked before writing, including JSON escaping and occurrence
contexts. The episode response budget is 32 KiB.

Bodies are opt-in. Follow a returned `next_offset` with `--body --offset N`;
no command automatically reads the entire diary. Context recall does not include
bodies.

## Correct the account, preserve the original

`episode_id` is the stable account identity. `edition_id` names one immutable
edition; initially they are the same ID.

To correct an account, supply a new source key, the current `expected_edition_id`,
a reason and the complete replacement content in `correction.json`:

```json
{
  "source": {"namespace": "experiment", "key": "sample-app/retry-test-correction", "reference": "journal://sample-app/retry-test-correction"},
  "expected_edition_id": "<current edition ID>",
  "reason": "Clarify which retry caused the duplicate",
  "summary": "A new operation ID duplicated the write; the original ID replayed it.",
  "body": "The duplicate appeared only after retrying with a different operation ID.",
  "thread": "sample-app",
  "occurrence_contexts": [{"namespace": "project", "key": "sample-app"}]
}
```

```sh
mnemed episode revise EPISODE_ID --input correction.json
```

Revision is full replacement, not a JSON patch. Omitted optional fields reset;
in particular, omitted occurrence contexts mean unknown, not inheritance.
A stale expected edition refuses instead of overwriting a concurrent correction.
Exact committed retries return their original edition, even after another revision.

`episode get EPISODE_ID` reads the current account; add `--edition-id ID` for a
historical edition. Ordinary `get ID` reads that exact node. Original bodies,
sources and evidence links remain available.

An event that changes the situation is usually a **new episode**, not a correction.
Correct a misspelling; append the later discovery that changed an earlier belief.

## Connect experience and learning

Episode SAVE accepts up to eight explicit authored `links`, in the same format
as note SAVE. A lesson can link to an edition with `derived_from`. Revision never
moves those evidence endpoints to the new account automatically.

```sh
mnemed episode references NODE_ID
```

References inspect incident links in both directions. The anchor may be a semantic
node or exact episode edition; a root read does not aggregate every edition's links.
Reading scenes does not train semantic associations or authorize reflection.
Automatic semantic decay, feedback, consolidation and pruning leave episodes and
their evidence links alone. Generic episode merge/erase is unsupported.

## MCP and remote use

Use MCP `save` with an explicit logical database:

```json
{"db":"project","kind":"episode","summary":"The retry test reproduced a duplicate write.","operation_id":"retry-scene-1","occurrence_contexts":[{"namespace":"project","key":"sample-app"}]}
```

The grouped `episode` tool handles bounded reads and revisions:

```json
{"db":"project","action":"list","thread":"sample-app","limit":8}
```

Reads are available to read-only profiles. Append requires curator; episodes
reject `core`. Revision requires operator. Receipts identify the root, edition,
revision, replay status and database, without echoing bodies.

The compatibility `episode append --input` route retains its native request
shape, which omits SAVE's `kind` field. Do not pass a SAVE envelope to that parser.
CLI local/remote and MCP share validation. [Remote CLI](remote-cli.md) selects a
direct owner; unsupported SAVE fails before submission without legacy fallback.
Library snapshots preserve editions and bodies, but pooled episode timeline/search
is unsupported; ordinary library semantic search remains semantic.

## Existing persistent stores

Current stores use **TouchstonesV1 / touchstones-v1, JSON v5**. Normal open never
upgrades a predecessor. Older readers/writers must refuse unsupported generations
rather than drop metadata.

`single-graph-upgrade` makes a detached successor in four named steps:

| Input | Target flag | Output |
| --- | --- | --- |
| capture-v1/episode-v1 SQLite or flat JSON v1 | Default `single-graph-v1` | single-graph-v1 / JSON v2 |
| single-graph-v1 / JSON v2 | `concern-v1` | concern-v1 / JSON v3 |
| concern-v1 / JSON v3 | `episode-context-v2` | episode-context-v2 / JSON v4 |
| episode-context-v2 / JSON v4 | `touchstones-v1` | touchstones-v1 / JSON v5 |

These are exact predecessor transitions, not shortcuts from arbitrary or managed
generations. Stop the owner and preserve paired database/body backups plus its
matching runtime before migration. Use absent absolute output paths in existing
directories:

```sh
# Only for a capture-v1 or episode-v1 predecessor:
mnemed --db /absolute/old.db single-graph-upgrade --backend sqlite --output /absolute/single.db

mnemed --db /absolute/single.db single-graph-upgrade --backend sqlite --target-generation concern-v1 --output /absolute/concern.db
mnemed --db /absolute/concern.db single-graph-upgrade --backend sqlite --target-generation episode-context-v2 --output /absolute/context.db
mnemed --db /absolute/context.db single-graph-upgrade --backend sqlite --target-generation touchstones-v1 --output /absolute/new.db
```

Use `--backend json` for the corresponding named JSON predecessor. Each step
retains database identity, IDs, bodies, durable records and historical source
proofs, leaving the source intact. It does not invent missing occurrence contexts.
The first step makes old Candidate nodes ordinarily searchable. Local `fs://`
body files are copied and verified; other valid body references are preserved
without fetching external contents. Volatile feedback receipts reset.

The output is not activated automatically. Verify it before changing owner
configuration; retain the source and prior runtime for recovery. Writers and
snapshot readers need matching current runtimes. Do not downgrade post-migration
writes by swapping only the binary.

### Meaning fingerprints are a versioned format

Current meaning fingerprints use `mneme.routing-content.v2` and
`mneme.routing-edge.v2` canonical codecs. Historical source/request digests are a
separate contract and stay unchanged.

Migration preserves old concern rows and routing-witness bodies but does not
relabel their old hashes as fresh evidence. Earlier contextual judgments lose
automatic routing applicability until rechecked. Stored weights and accumulated
native learning survive; stale hints are neutral, not negative feedback. The
upgrade report discloses this transition. Concern findings remain a latest-scoped
cache, not an unlimited archive; retain the predecessor for its older records.

`reembed` / `reindex` rebuilds vectors for all canonical nodes, including archives
and historical editions, through a detached replacement. The persistent route
keeps a backup beside the source and refuses an existing backup. It uses bounded
inference batches and an O(N) in-memory export: plan it as offline maintenance,
not a recall operation.

## Agent practice

Keep selected episodes for remembered experiences and semantic notes for reusable
learning. Future work and ongoing journals can refer to them without becoming
the same thing. Write selectively; no transcript import, mandatory lesson or
startup diary dump is implied.

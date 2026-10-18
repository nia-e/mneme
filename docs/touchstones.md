# Touchstones

A touchstone records why something matters to a named subject and keeps a
historical summary of what it referred to. It is a semantic note with typed
references, not a separate database or an importance score.

Use one for deliberately authored significance that should remain discoverable
outside the startup core set. Retrieval frequency does not create that meaning or
make a note core.

## Decide what matters

At natural checkpoints, consider what matters to you as well as what would be
useful later: a shared scene, an experiment, or a changed understanding can merit
a touchstone without yielding a reusable lesson. The foreground agent decides
and authors its own meaning in first person. Name that subject explicitly;
claims about the user or other people still need their own evidence. Notify the
user briefly when worthwhile, rather than asking them to approve your meaning.

A read-only background worker can suggest a candidate and its source scene; the
foreground agent accepts, revises or declines it. A nomination is neither an
authored touchstone nor a prerequisite: the foreground can author directly under
existing selected-owner write authority. Do not manufacture significance to fill
an empty collection, or turn significance into automatic core membership.

The Codex integration can retain a recorder's `touchstone_nomination` in a bounded,
durable local pending handoff and offer it at a later prompt/tool boundary in the
same session. Emitted means offered, not seen, approved or saved; the proposal is
not a memory. Do not substitute `touchstone`-tagged writes for nominations.
Background rediscovery of an existing touchstone does not authorize rewriting
its meaning. Check installed handoff support rather than treating these source
instructions as deployment; see the [Codex integration](../integrations/codex/README.md).

## Other foreground memory judgments

The same bounded local handoff also carries optional `review_nomination` data:
core creation, promotion/demotion, changed core wording, supersession, forgetting,
contradiction or merge adjudication, and episode corrections. A nomination names
only offered same-owner targets or proposes new wording from cited source evidence.
It is not a native request, approval token, automatic action or durable to-do list.
Ordinary background recording and permitted meaning-preserving cleanup continue;
[per-owner core cleanup](declarative-config.md#independent-controls) can be restricted
to the foreground. A small text change is not proof of unchanged meaning.

The foreground independently inspects the evidence, freshly GETs exact targets,
and decides whether to author an ordinary operation under its existing authority.
Use that operation's real guards: complete-tag/content checks for membership,
summary/body guards for edits, and exact edition expectations for corrections.
Touchstone immutability still applies. Older ID-only destructive/adjudication
operations do not gain atomic stale-decision protection from the handoff; a fresh
read followed by a write is not compare-and-swap. Ambiguous acknowledgements
require inspection, not automatic retry. This workflow grants neither new user
permission nor authority over services, leases, backups, migration, decay or prune.

Like touchstone suggestions, review nominations are optional, same-session and
at-most-once offers. Emission is not receipt, approval or execution. Earlier
Codex/Claude touchstone nominations keep their original source-coverage claims;
Claude snapshot nominations retain their observed horizon and unproven finality.

## Author one

If a nomination cites only a source, inspect it first. If the scene or note is
worth preserving, deliberately SAVE it in the same authorized owner, then GET its
exact node or episode edition as below. A nomination does not trigger an automatic
scene write; never manufacture a target merely to satisfy the reference contract.

In the foreground, freshly GET each exact target, including targets suggested by
a background worker. Copy its `db_id`, exact `id` and
`summary_snapshot.expected_snapshot_sha256` from GET. Then use ordinary
`save --input touchstone.json` or MCP `save`:

```json
{
  "summary": "That design session helped me find a way of working I want to keep.",
  "tags": ["touchstone", "agent-continuity"],
  "touchstone": {
    "subject": "<agent continuity identity>",
    "references": [
      {
        "db_id": "<logical database ID>",
        "id": "<exact node or episode-edition ID>",
        "expected_snapshot_sha256": "<GET-issued digest>"
      }
    ]
  }
}
```

For MCP, add the explicit `db`. All targets must belong to that store; cross-store
references are unsupported. Copy the native digest, not a hash you compute from
prose. GET's separate `coverage` field is readback metadata, not a SAVE argument.
A `touchstone` tag alone does not create a typed annotation.

Normal [SAVE identity and retry rules](cli-and-mcp.md#save-and-retry) apply.
The first write checks target summaries and saves the annotation and snapshots
atomically. Exact retry returns the committed annotation even if a target changed
or disappeared. To change the meaning, author a new note with a new source/operation
identity and supersede the old one where appropriate.

## What survives

References retain summary-only snapshots: exact database/node identity, summary,
provenance, creation time and memory-kind metadata. Episode references name the
particular edition, not its current head. GET does not inject calendar strings
into these immutable snapshots. Separate `touchstone_references_utc` rows, keyed
by database and exact node/edition ID, render their dates in whole-second UTC;
canonical numeric timestamps and snapshot hashes remain unchanged.

- Archiving a target does not invalidate the reference.
- Editing, merging or deleting it does not retarget the reference or erase the
  snapshot. GET shows historical content separately from current resolution.
- An unchanged snapshot means summary-projection equivalence, not unchanged body
  content. Touchstones do not archive target bodies.
- Generic overwrite and full merge cannot rewrite authored annotation content.
  Lifecycle and retrieval telemetry can still change; explicit supersession keeps
  the old annotation available.
- Deleting an annotation removes its own metadata, not its targets or other
  annotations' references.

## Find and browse

Ordinary context retrieval can discover touchstones from their cited scenes or
show historical summaries behind retrieved touchstones. Expansion is one hop
from original hits. References share the foreground byte budget; omissions are
reported rather than treated as reads.

Browse the indexed collection:

```sh
mnemed list --touchstones --limit 8
mnemed list --touchstones --limit 8 --after CURSOR
```

MCP uses `list {db:"project", kind:"touchstones", limit:8}`. Follow `next_cursor`
with `after`, keeping the same selected store. CLI pages are JSON even without
`--json`; limits are 1–32. A full page offers conservative continuation, so the
last page may be empty. `has_more` means another request is available, not that
another row is guaranteed.

For visual browsing, open [Observatory Touchstones](observatory.md#touchstones).
Project-only recording retains its configured scope; touchstones do not grant
access to private user memory or enable a new background process.

## Scope boundary

Cross-store references, full-body archives, automatic successor selection and
automatic personal-meaning edits are unsupported. Check the selected owner's
catalog for touchstone support before use.

Current native storage and JSON export use a successor format. Older writers
must refuse it rather than discard metadata; normal open does not upgrade stores.
See [migration steps](episodic-memory.md#existing-persistent-stores) and the
[memory model](memory-model.md).

## Verification

Before migrating a retained store, test reference roundtrips and reopen behavior
on a disposable copy with the intended runtime and backend. Preserve paired
database/body backups and a matching prior runtime for recovery. Reference checks
establish mechanics, not improved personality continuity.

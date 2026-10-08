# Touchstones

A touchstone records why something matters to a named subject and keeps a
historical summary of what it referred to. It is a semantic note with typed
references, not a separate database or an importance score.

Use one for deliberately authored significance that should remain discoverable
outside the startup core set. Retrieval frequency does not create that meaning or
make a note core. Background recording may rediscover a touchstone, but does not
rewrite its meaning or infer someone else's feelings.

## Author one

Read each target first. Copy its `db_id`, exact `id` and
`summary_snapshot.expected_snapshot_sha256` from GET. Then use ordinary
`save --input touchstone.json` or MCP `save`:

```json
{
  "summary": "The team values the design session for clarifying a boundary.",
  "tags": ["touchstone", "agent-continuity"],
  "touchstone": {
    "subject": "project-team",
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
particular edition, not its current head.

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

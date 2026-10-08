---
name: mneme-reconcile
description: >-
  Curate and reconcile mneme long-term memory: resolve contradictions (triage a
  flagged pair as superseded / context-dependent / unresolved), merge redundant
  nodes, make guarded tag/body/summary corrections, decay pending edge interference,
  and forget what's wrong. Trigger
  on "these two memories conflict / contradict", "reconcile contradictions", "merge
  duplicates", "clean up / curate memory", reviewing ordinary memories, or a
  maintenance pass. Builds on the `mneme` skill (how to drive it, selected owner scopes).
---

# mneme-reconcile — curate and resolve

Deliberate curation, separate from ordinary remembering. Native policy controls
what can change; the agent supplies evidence and judgment. The source librarian
can record advisory findings, but cannot silently merge or supersede memories.
See the `mneme` skill for the tool reference.

MCP status, contradiction triage, and merge-candidate listing are `read-only`.
Flagging a contradiction and ordinary retagging are available to `curator`;
summary/body edits, adjudication, merge, maintenance and forget require `operator`.
Either tag set containing `core` also requires operator. Capability does not grant
authority to change content or scope.

Use the selected existing owner first, through MCP or the owner-routed ordinary
CLI. The CLI does not bypass the owner's profile or acquire another live-store
lease. Profiles are fixed at startup. Only an explicit offline `--db` command or
separate operator process needs an operator-managed lease handoff; a curator
cannot release or elevate in place. Do not stop services or widen profiles merely
to inspect or attempt ordinary curation.

For a reconciliation or maintenance pass, **start with `status{db}`** (CLI:
`mnemed status`) — it reports, per selected db, the open
contradictions, open merge candidates, and pending **edge** decay
(banked edge interference a sweep would act on; `0` ⇒ a sweep would no-op). Run only the
passes that matter to the current curation task. These counters do not enumerate
the source prototype's advisory cases.

## In-place editorial corrections

Choose the smallest supported edit in the same owner; do not duplicate, archive or
supersede a valid note just to change its wording or tags. Inspect the installed
catalogue and current GET first, retaining the database ID and exact native guard:

| Change | MCP request fields after `db`, `expected_db_id`, `id` | Ordinary CLI |
|---|---|---|
| Complete tag set | `retag{…, expected_tags, tags}` | `retag ID --expected-tags … --tags …` |
| Complete body | `edit_body{…, expected_body_revision, body}` | `edit-body ID --expected-body-revision REVISION --body-file FILE` |
| Searchable summary | `edit_summary{…, expected_snapshot_sha256, summary}` | `edit-summary ID --expected-snapshot-sha256 HASH --summary "…"` |

Retag uses all current tags, not a topology prefix. Body revision is GET's opaque
`body_revision`, not a content hash; read the body and follow ranges when needed.
Summary guard is `summary_snapshot.expected_snapshot_sha256`; a successful edit
atomically refreshes search projections. Configured CLI owners inject the identity
guard; explicit `--remote` also needs `--expected-db-id`.

Retain unrelated tags, original provenance and authored voice; follow
[mneme-remember's voice guidance](../mneme-remember/SKILL.md#keep-authored-voice-and-history-distinct).
A changed conclusion may warrant a sourced successor and explicit supersession
instead of an editorial rewrite.

Episode editions use `episode revise`; authored touchstone summary/body edits and
body edits with outgoing span anchors are refused. These routes promise neither an
edit-history ledger nor body erasure. Stale guards require inspection and new intent;
a lost acknowledgement stays ambiguous, never a blind retry. Read back before
claiming success. Unsupported owners and read-only copies refuse without a
replacement SAVE. See [the edit contracts](../../../docs/cli-and-mcp.md#edit-tags-in-place).

## Encountered concerns — preserve the distinction

ConcernV1 and its successors provide `concern` with `list`, `notice` and
`record_finding` actions. Listing is read-only; notice/finding requires curator.
Check the installed catalogue first. This is separate from the legacy
contradiction-counting workflow below, not a second way to adjudicate history.

Keep both claims and ask what scope or observation would distinguish them. Bind
notices to native GET-issued endpoint meanings; a finding compares the complete
expected row and records a scoped observation with source/digests. Old findings
are historical evidence, not current-state guarantees. A partial correction must
not supersede a multi-claim node. No decisive evidence means no write.

The automatic loop uses already-inspected cards, a paired caveat in the ordinary
memory packet, and the existing close assessor. It needs independent task
evidence, not merely an actor agreeing with the memory. Findings need no new
lesson; notes and findings can succeed independently. See the
[joined contract](../../../integrations/codex/README.md#lazy-advisory-curation).

## Authored touchstones

In the [touchstone design](../../../docs/touchstones.md), an annotation's
personal meaning and historical references are deliberately authored. Do not merge
or rewrite the annotation through automatic maintenance. A target may become
obsolete without becoming unimportant. Snapshot references survive target changes;
current resolution is separate and summary-only, not evidence of unchanged bodies.
Surface a relevant caveat lazily; no repair is required merely because a target was
archived. New interpretation means a new authored note and explicit supersession.

## Contradictions — flag, triage, resolve

1. **Flag** whenever two nodes plainly conflict: `contradict{db, a, b}` (curator) — cheap, and
   it bumps an observation count on the pair. (Often done while reading/recalling.)
2. **Triage** the open set: `contradictions{db}` (CLI: `mnemed reconcile`) returns
   open contradictions ranked by observation count, **each tagged with its nodes'
   current communities**, plus a derived "community X conflicts with community Y"
   aggregate — so you can see whether a clash is one-off or two whole topics
   disagreeing.
3. **Decide** each pair under operator authority:
   - `supersede{db, winner, loser}` — a real conflict, newer replaces older: emits a
     Supersedes edge and archives the loser, preserving direct history reads while
     excluding it from ordinary recall and core loading. Replay is a historical
     no-op, not a way to repair later mutations or old records.
   - `reconcile{db, a, b, resolution:"context-dependent"}` — both true in different
     contexts; stop flagging it.
   - `reconcile{db, a, b, resolution:"unresolved"}` — leave it standing (e.g. for a
     human).

`unresolved` is an explicit deferral, not a terminal verdict: the pair remains in
the open triage set and may later become `superseded` or `context-dependent`. Those
two verdicts are terminal. Supersession is one atomic, direction-idempotent historical
event per canonical pair; replay does not reassert status or topology changed by a
later legal mutation.

## Merges — collapse redundancy

`not-new` feedback during recall banks **merge candidates**. Drain them:
`merges{db}` lists the open pairs; `merge{db, mode:"…", …}` resolves each —
`full` (collapse the loser into the winner) or `keep` (decline; they're distinct
after all). Listing is read-only; either resolution requires operator.
The exact MCP shapes are `merge{db, mode:"full", winner, loser}` and
`merge{db, mode:"keep", a, b}`.

Default MCP does not expose unreceipted direct feedback or an explicit
**propose merge** operation, so it cannot manufacture a candidate from an arbitrary
pair during curation. `merges` enumerates candidates already in the store. The CLI
compatibility command `mnemed feedback not-new --from A --to B` can bank one; a
first-class receipt-bound/explicit proposal boundary remains roadmap work.
The CLI needs no MCP-only `--allow-direct-feedback` startup flag. An owner-routed
command still needs the owner to advertise/admit direct feedback; explicit offline
`--db` requires its exclusive lease. Do not weaken a host for this compatibility path.

Resolution is terminal/idempotent: repeating the same verdict is a no-op and a
different verdict conflicts. Full merge is one operation-bounded local transaction
with keyed linear retry history, but it does not yet redirect/block other open
overlays naming the loser. The unsafe legacy partial-merge writer has been removed;
persisted `Partial` verdicts remain readable, and any future replacement must commit
its child and both derivations atomically.

## Maintenance passes

Every MCP operation in this section that mutates state requires operator. The
owner-routed CLI respects that same owner capability; explicit offline maintenance
requires matching user authority and the exclusive lease.

- `decay{db}` / `mnemed decay` — charge banked **edge** interference; this does not
  demote or archive nodes.
  Idempotent (a second run with nothing emitted between is a no-op).
- `prune{db}` / `mnemed prune` — delete unvalidated weak edges and enforce the
  configured physical association-degree cap.
- Inspect ordinary active memories or explicit archived history when curation
  calls for it. `query` is a bounded semantic lookup, **not** an inventory or
  evidence that every memory was reviewed. Retire wrong or obsolete claims by
  explicit archive/supersession/forget decisions, not lack-of-use telemetry.
- `forget{db, id}` (operator) — drop a wrong/unwanted node and its local vector/edges. Bodies
  created by this database are erased only after the final local reference;
  explicit/legacy borrowed references are never deleted. Edge decay does not archive
  nodes.

## Scale judgment to the task

A bounded read-only hygiene pass can report source/edge concerns without deciding
truth. Conflict detection still needs scope and evidence; adjudication needs enough
reasoning to justify the historical change. Use the available model appropriately,
not a mandatory weak/mid/strong pipeline. Drive ordinary recall walks locally;
delegate only when authorized and useful, as described in mneme-recall.

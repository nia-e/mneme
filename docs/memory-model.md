# Memory model and context contracts

Mneme stores reusable notes and accounts of events, with links between them.
Search finds relevant material; explicit curation decides what to preserve,
correct or retire.

## Notes, episodes and touchstones

- **Semantic notes** hold facts, decisions, lessons and working assumptions.
  New notes are active and searchable. Archived notes remain inspectable but
  are excluded from ordinary search and traversal until restored.
- **Episodes** hold selected accounts of what happened. Each account has immutable
  editions, separate recording and occurrence times, and optional references to
  lessons. A current edition is the latest edit, not proof of current reality.
  See [episodic memory](episodic-memory.md).
- **Touchstones** are semantic notes with an authored explanation of why material
  matters to a named subject. Their references retain historical summary snapshots
  instead of following later edits or successors. See [touchstones](touchstones.md).

Tags provide exact, case-sensitive filters. The `core` tag selects notes for
explicit startup loading: identity, project orientation or working assumptions.
Core membership is curated, not earned by frequent retrieval. Core notes also
participate in ordinary search; they are not automatically archived.

The MCP `core` response is `{nodes,total,truncated}`, with at most 32 nodes and a
shared 128 KiB body allowance. These limit one response, not how many core notes
may exist.

## Recall and context

Use CLI `recall-context` or MCP `recall_context` for model-ready context.
They produce a byte-bounded `mneme.context.v7` JSON envelope containing semantic
cards, a separate episode section, touchstone references and coverage information.
The default budget is 32 KiB; it includes JSON encoding and omission metadata,
not just note text. Bodies and feedback receipts are not included.

Episode discovery combines lexical matches with one-hop exact references.
Historical references retain their edition; they never silently become the
current account. Touchstone references also expand only one hop. Read coverage
and omissions before treating an empty result as evidence that no memory exists.
Retrieval partiality and content omitted to fit the response are reported separately.

`query` is the diagnostic raw-search interface (`mneme.query.v3`), not the same
context envelope. MCP bounds search seeds, traversal depth and hydrated nodes.
Explicit body reads default to 64 KiB, cap at 1 MiB, and return `next_offset` and
`has_more`; aggregate prefix responses may instead report `body_truncated`.
Each call is bounded, but repeated calls still accumulate work and context.

For protocol limits and transport setup, see [CLI and MCP](cli-and-mcp.md#as-an-mcp-connector--recommended-for-agents--chat).

## Reading and learning

Reading does not reinforce a memory, create feedback or change graph topology.
Not seeing or using a note is not negative feedback. Search relevance, authored
significance, observed usefulness and historical scalar metadata are different
things; none is a truth score.

A read-only walk can return a server-issued receipt. A separate `reflect` call
uses that receipt to report which visited memories actually contributed.
Grounded-use telemetry does not automatically archive or promote notes.
See [deliberate learning](cli-and-mcp.md#agent-workflows) for receipt lifetime and
retry rules.

## Contradictions and merges

Contradictions and merge candidates are review queues, not traversable links.
Ordinary retrieval does not spread across a flagged conflict.

A contradiction can be **unresolved**, **context-dependent** or **superseded**.
Unresolved remains open for later review; the other verdicts are terminal.
Supersession atomically records the direction, adds its edge and archives the
loser. The archived note stays available for exact history reads but leaves
ordinary recall and core loading.

A full merge normalizes local incident links, archives the loser, resolves the
candidate and records an exact-pair retry proof in one transaction. It is local,
not a cross-database merge or a general redirect. Other review overlays involving
the loser are not redirected. Partial merge is unsupported; historical partial
verdicts remain readable.

Repeated committed pair resolutions are no-ops; changing a terminal verdict
conflicts. Their retry proofs record historical operations, not permanent graph
invariants: later legal edits are not undone by replaying an old resolution.

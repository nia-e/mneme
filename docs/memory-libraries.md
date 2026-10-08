# Memory libraries

Use an optional memory library to search several explicitly enrolled stores in
one request. Each store keeps its own owner; the library coordinates read-only
queries rather than combining databases. A standalone store does not need one.

This guide covers library reads and their ownership, snapshot and isolation
contracts. For initial configuration and enrollment, see
[publisher setup](../integrations/library/README.md).

## Interfaces

```sh
# Read a library without opening another copy of an owner's database.
mnemed library catalog
mnemed library query "why did we change this?"
mnemed --json library recall-context "current task"

# Copy the returned project, node, device and optional generation reference.
mnemed library get PROJECT NODE DEVICE --generation GENERATION

# Create a coherent bundle through an already-running operator host.
mnemed --remote http://127.0.0.1:18765 --remote-db project snapshot create

# Standalone coordinator, or add --http for an existing-client service.
mneme-mcp --library-config /absolute/personal/library.json
```

CLI config selection is `--config PATH` (explicit override), then the nearest
ancestor `.mneme/profile.json`'s `library_config`, then
`$XDG_DATA_HOME/mneme/libraries/personal/library.json` (or
`~/.local/share/mneme/libraries/personal/library.json` without `XDG_DATA_HOME`).
A missing or invalid selected config is an error, not automatic enrollment or
creation. Isolated profiles need their own independent library and never fall
back to the personal one. Older nonstandard install paths still work through
`--config PATH` or a project profile. Native MCP keeps its explicit
`--library-config`; the Codex launcher handles its profile selection.

The coordinator exposes only `library_catalog`, `library_query`,
`library_recall_context` and `library_get`. Operator owners separately expose
`snapshot_create`; the explicit CLI adapter requires `--remote`. Snapshot
creation is not a competing offline file-copy command.

`library_recall_context` asks each selected source for ordinary bounded context
and pools semantic notes plus a clearly marked `episodes` lane. Its
`mneme.library.context.v5` envelope retains one 32 KiB budget and source-pinned
edition references; `library_get` reads the exact cited account. Episode matches
use lexical or explicit reference retrieval and never enter the semantic reranker.
Compact touchstone facets and discovery coverage are reported separately.
Coverage distinguishes no matches and skipped tag filters; older owner envelopes
are refused rather than treated as successful empty coverage. Both lanes use the
same chosen live owner or fallback snapshot, never a silent mixture of ages.
Diagnostic `library_query` remains semantic-only under `mneme.library.query.v2`.

Library configuration and catalog use separate versioned JSON documents.
`rerank: false` explicitly selects source-grouped reads; absent reranking support
also degrades visibly. Configured inference is lazy, process-resident and admits
one model job at a time. See [publisher setup](../integrations/library/README.md)
for the JSON shapes, enrollment, peer transport and timer examples, and
[Codex integration](../integrations/codex/README.md) for the profile-gated MCP
launcher. Registering a personal coordinator directly in global Codex settings
would bypass that profile boundary; use the launcher there.

## Model

A library is an explicitly configured collection of stores. Each store still has
one authoritative owner. A read-only coordinator asks the owners directly and
falls back to a verified snapshot when an owner is unavailable. The coordinator
does not open the owners' databases or become another writer.

An always-on remote owner is optional; a library can be entirely local.

The library index records project identity, owner device, database identity and
registry alias. Connection addresses are local configuration: the same owner can
be reached through loopback on one device and SSH on another. Owners announce
enrollment and later descriptor revisions to explicitly configured peers. An
announcement can arrive before its first snapshot; “known, no mirror” is a real
state, not an empty database.

Routes can be keyed by `(owner_device_id, db_id)`: two Codex projects on one
computer usually have different MCP ports even though both use the alias
`project`. A descriptor announcement does not magically create a network route.
Remote peers report a newly discovered project as **unrouted** until a route is
configured or a snapshot arrives. A single-device owner endpoint remains useful
when one native host intentionally serves several registry aliases.

## Reads

The coordinator exposes a catalog, diagnostic query, bounded model context and
provenance-aware node lookup. Single-store commands keep their existing meaning.

- Select one source per store. Prefer its live owner, then a matching snapshot on
  availability failure. Authentication, database-identity and protocol failures
  are not silently converted into stale answers.
- Retrieve bounded summary candidates without bodies or exposure credit. Rerank
  the pooled candidates together when a shared reranker is available. Independent
  stores' first-place results are not comparable relevance scores.
- Without that reranker, show source-grouped results and explicitly say global
  ordering is unavailable. This is useful degradation, not equivalent quality.
- Preserve primary results, source labels, snapshot timestamps and
  partial-coverage information inside one bounded context envelope.
- Follow-up reads retain source/generation identity. If that generation is no
  longer available, return an expired reference rather than substituting data.

## Publication

The owner-native snapshot operation briefly fences the store, checks quiescence,
and retains its exclusive lease while copying its checkpointed database and
referenced portable bodies. It closes and reopens the prepared backend before
network transfer. A checkpoint-prepared backend cannot simply be reused.

Active requests, background jobs, walks and unconsumed feedback receipts can defer
a snapshot. The publisher skips busy stores; it does not discard their state to
meet a timer. A configured timer is a best-effort freshness target, not a freshness guarantee.

Snapshots have an inventory and content hashes. Transfer goes through private
staging; only a verified complete generation can be activated. Replica serving
uses separate writable serving copies behind a read-only-profile MCP process.
Never replace a database under an open backend. Keep the previous verified
generation for recovery and prune only recognized old generations.

Whole-generation replacement preserves deletions without merging forgotten notes
back into the graph. Withdrawal records prevent old announcements from enrolling
a project again. Disconnected copies cannot promise immediate erasure.

V1 does not provide write-anywhere synchronization, ownership transfer, CRDTs,
cross-store graph traversal, or arbitrary legacy/managed-store migration.

## Privacy and isolation

Enrollment is explicit for existing stores. A configured personal library may
opt new normal projects into sharing; Mneme itself never silently enrolls arbitrary
directories. A private project is not shared. An isolated library/profile also
excludes personal core, personal-library tools, automatic recall and publication.

Hooks and launchers resolve the profile before contacting memory services. Startup
loads the selected library's global core, then the current project—not every
project's core. Without library/profile configuration, existing behavior remains.

An isolated profile cannot remove personal context already inherited by a running
agent. Experiments must start a fresh isolated session, not merely disable writes
or fork an already-personal conversation.
This boundary covers Mneme's hooks, tools and publication; a harness's separate
built-in memory or injected instructions must also be isolated by that harness.

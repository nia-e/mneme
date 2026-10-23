# Hippocampus stewardship

The first slice is implemented; rollout and validation are tracked separately
from this operator guide.

The background librarian records useful experience, brings relevant context
forward, and maintains the memory it works with. Routine upkeep should disappear
from the main conversation. The main thread gets useful information, or a consequential
question that needs the user's input. Ordinary uncertainty can remain a quiet
abstention. Its work remains inspectable when wanted.

The first slice gives that librarian a shared tag vocabulary, optional owner-owned
guidance, and an asynchronous tag-maintenance pass. Broader reconciliation and
merging belong to the same role; this slice does not implement those operations.

## Design for future questions

The important question is **“What would this distinction help us find later?”**
Consistent spelling is the bookkeeping underneath that judgment. A useful tag can
preserve a rare connection between topics, distinguish expertise from incidental
mention, or group experiences that answer a similar future question.

For a Rust people/project graph, `people` can identify a person as a substantial
subject: biography, expertise or roles. Mentioning a person in an API discussion
is different. An exact handle can accompany `people`; role and topic tags answer
other questions. Historical roles remain historical, and tags do not establish
current membership.

Observed vocabulary is evidence, not an ontology. Counts identify common spelling
or possible fragmentation; they do not decide usefulness. `compiler` and
`rust-compiler` may overlap without being interchangeable. A rare identifier may
be exactly the filter someone needs. New connections still need support in the
memory: prospective usefulness is not permission to invent facts or significance.

## Canonical spelling

New tag materialization uses only ASCII `a-z`, `0-9`, and `-`. ASCII uppercase
becomes lowercase; spaces, `_`, `:`, `/`, `.`, and `-` become one collapsed dash,
with leading/trailing separators removed. For example, ` Rust:Libs ` becomes
`rust-libs`. Spellings that normalize to the same tag collapse into one member.
Unsupported characters (including Unicode and controls), empty results, and
resource-limit violations are errors, not guessed transliterations or aliases.

Replacement tags use this policy; `expected_tags` remains the exact observed set
for compare-and-replace. Source-keyed capture and episode replay proofs retain
submitted spellings, not normalized spellings. An exact replay checks that original
proof before attempting new materialization; changing spelling on a retry changes
the submitted request even when it would produce the same canonical tags.
Persisted legacy tag sets require the [explicit migration and writer fence](tag-normalization.md); source
changes alone are not permission to open an unmigrated live store.

## Discover the vocabulary

The existing inventory operation also lists tag names:

```sh
mnemed list --tags
mnemed list --tags --prefix pe --status active
mnemed list --tag people
```

The corresponding MCP request is:

```json
{"db":"project","kind":"tags","prefix":"pe","status":"active","limit":32}
```

Use the returned `next_cursor` as `after`, retaining prefix and status. Prefixes
are exact and case-sensitive; identifiers are not silently normalized. Pages
report their work and whether more names may remain. Counts are marked `exact`,
`lower_bound` or `unavailable`, and examples are bounded node IDs rather than
representative samples.

Vocabulary browsing uses the semantic tag index. It excludes episode editions,
reads no bodies, and performs no inference or reinforcement. It is not a snapshot:
concurrent changes can require a fresh pass. Tag-filtered browsing reflects present
membership, which may still be inconsistent. Ordinary semantic recall does not
acquire guessed tag exclusions.

## One owner, one guide

Automatic capture and maintenance share built-in guidance about future questions,
supported connections, identity and uncertainty. An owner can also select an exact
guide node with `tag_guide_id`. Its canonical summary holds the complete guide,
at most 2 KiB: preferred names, intended meanings, useful queries, examples,
exclusions and any genuine equivalences. A body can provide background, but is
not policy input. Keeping the guide in the canonical record lets a transaction
guard the actual text used for classification.

A `tag-guide` tag can help people discover a note, but it does not make that note
policy. The configured node ID selects the guide in the same database. Its
summary must be read completely; a missing, invalid or unavailable configured
guide defers dependent tag work while ordinary recording remains available.
An archived guide can remain selected: explicit configuration, not recall status,
determines this use.

Guide content and observed membership have separate fingerprints. Vocabulary
growth does not by itself invalidate classifications. A guide-content change makes
prior classifications eligible for another look. Manual writers can use the same
vocabulary and inspect the configured guide; native saves still accept deliberately
authored tags rather than running a hidden classifier.

## Capture and upkeep

**At capture**, the existing off-thread assessment sees bounded vocabulary and
guide context and proposes ordinary topic tags alongside its note. This adds no
model call per tag. The host validates the tags and retains its own special tags;
the model cannot manufacture core membership or rewrite authored significance.
The capture receipt retains the guide revision used. Capture is not a transaction
against the whole guide: later maintenance can reconsider it under changed policy.

**During idle work**, the same isolated model runtime examines bounded batches.
Recently recorded or changed material and a resumable traversal of cold nodes
share the maintenance allowance. A durable hot/cold turn governs native-read
and model-packing order across sessions, so a long cold prefix cannot consume
every byte slot. No-op and unresolved classifications advance examined coverage
too. The initial classifier uses target summaries, not mutable external bodies;
insufficient context warrants leaving tags alone.

Coverage means “examined against this content and guide revision,” not “correct
forever.” Unchanged unresolved material does not need another model argument on
every wake-up. New evidence or policy can reopen it. The traversal cursor and
classification validity are separate, so a guide change does not repeatedly send
the sweep back to the first node.

Edits use native guarded `retag`: the complete prior tag set, target-content
fingerprint, and any guide fingerprint must still match in the write transaction.
A stale proposal is discarded and reassessed. The same operation is available to
manual callers through CLI and MCP. Existing tag-only callers retain their weaker
complete-tag-set comparison; automatic maintenance requires the stronger contract.

The owner-local journal records examined revisions, usage reservations and edit
outcomes. It is not a native commit receipt. If an acknowledgement is lost, that
operation stays unknown even when today's tags match its proposal. After a bounded
deferral, a fresh assessment can continue without asking the main thread to approve
routine work, but it cannot retroactively prove the earlier write succeeded.

## Operation and limits

Fresh automatic-recording preparations enable `tag_stewardship`. Existing
configurations without that field remain unchanged. Both project and misc
preparation support `--no-tag-stewardship` and optional `--tag-guide-id ID`;
recording-off configurations do not run maintenance. Follow the
[installation and update guide](../integrations/codex/README.md) when preparing a
new runtime. Do not edit a live bundle or reset usage ledgers to enable this work.

Paid maintenance charges the last eligible turn-end packet, protects foreground
recall, and also accounts inference against the unchanged device-local owner
allowance per UTC day. Without an eligible packet it cannot spend; idle wake-ups
do not mint allowances. Multiple sessions using the same database must not
multiply the owner/day allowance. Packet replay, restart, configuration changes
and compaction do not refill a packet. Lifetime session counters remain telemetry,
not admission limits.

Native reads, context bytes, inference and retries are bounded. Unknown provider
usage retains conservative spending evidence and fences model admission for that
owner-day and its owning packet. A new UTC day does not clear that packet's
uncertainty, and a new packet does not clear the owner-day fence.
The worker runs opportunistically through the existing lifecycle, not as an
always-running scheduler. A bounded connection, catalog validation and owner-identity
exchange precedes maintenance admission. Content batches then share a durable
owner-wide cadence: at least the configured native I/O window between admissions
(10/15/20 seconds for low/medium/high effort), with the existing aggregate byte
and read/write time envelopes per batch. Thus even continuously active sessions
using one effort admit at most `ceil(86400 / native_seconds) + 1` content batches
per UTC day; at medium effort that is 5,761 batches with a 512 KiB decoded-byte
envelope each.
This rate bound does not include the preceding identity exchanges, and is an
upper bound, not a promise of all-day operation. Idle guide probes back off up to
the existing five-minute worker lifetime; queued work bypasses that idle backoff,
not the shared cadence. Native rounds remain cumulative accounting, not a quota
borrowed from model attempts. Exhausting the owner model allowance blocks
inference and edits, but another eligible session can still make bounded
scan-only progress. Inference retains the runtime's separate 45-second deadline;
the native I/O window does not shorten it.

These are admission and accounting limits, not hard provider billing caps. The
per-packet low/medium/high thresholds remain 24/48/96 attempts, 125k/250k/500k input
tokens and 10k/20k/40k output tokens. Prompt and answer bytes, per-call bounds and
deadlines are unchanged; the provider can use more reasoning or output tokens than
reserved for the final admitted call. Missing usage fences further model spending
for the affected packet and owner-day rather than being counted as free work.

The auxiliary journal is indexed SQLite under
`$XDG_STATE_HOME/mneme/codex-stewardship/DB_ID.sqlite3` (default:
`~/.local/state/mneme/codex-stewardship/`). It is separate from the memory store.
Inspect it without starting work or creating a missing journal:

```sh
python3 /absolute/runtime/lib/stewardship.py --db-id DB_ID --limit 16
```

The report includes progress, usage and recent edit outcomes. Keep unknown or
unreadable state intact; deleting it also discards coverage and spending evidence.

This implementation preserves note identities, sources and bodies. Episode
editions and touchstone meaning are not retagging targets; core and other special
tags remain protected. Reads do not schedule reinforcement or mutate topology.
The librarian's freedom is to make supported, recoverable improvements within its
configured owner—not to cross into a private global store.

Focused provider-free tests can establish these boundaries, concurrency behavior
and recovery. They cannot establish that a model's chosen tags improve future
answers. That quality claim needs observed use, not a prettier vocabulary count.

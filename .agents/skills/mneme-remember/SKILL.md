---
name: mneme-remember
description: >-
  REMEMBER durable knowledge into mneme long-term memory: scan the session for
  facts, open questions, decisions, and hard-won gotchas worth keeping across sessions, save each
  as a scoped user-profile, agent-continuity/practice, project or ordinary-work node
  in its explicitly selected owner, and wire grounded associations.
  Trigger on "remember this", "save to memory", "note this for
  later", "don't forget X", or a deliberate checkpoint before context is compacted.
  Builds on the `mneme` skill (how to drive it, selected owner scopes).
---

# mneme-remember — write the task into memory

Choose the form before the tool: a **semantic note** can be reusable knowledge or
a clearly labelled open possibility;
an **episode** is a selected thing that happened. Facts, distilled lessons and
tagged possibilities use the same semantic note path; a possibility is not an
agenda subsystem or execution commitment. Keep durable open ideas and their chosen
status in these notes, not a second backlog in docs. A short execution handoff can
still preserve the exact active checkpoint; design documents retain rationale and
dated reports retain evidence, rather than duplicating open-item state.
Keep a scene because it matters, not because every turn owes the database a record. A shared moment or failed experiment can be worth retaining
without a lesson attached. Episodes help resume scenes, avoid repetition and refer
to shared events; reusable lessons remain semantic notes.

At a natural checkpoint — a decision reached, a fact established, **before context is
compacted** — deliberately scan the recent context
for what's worth keeping. Remember *durable* things: facts, decisions, hard-won
analysis, gotchas. Not transient chatter. (See the `mneme` skill for the tool
reference and selected owner scopes.)

This is the manual workflow. Separately authorized opt-in hooks or recorders may
prompt or record at their own boundaries; neither makes a memory write mandatory.
A reviewed device misc binding may cover genuinely unconfigured work, but known
store/skill discovery alone grants no automatic writes or private-global routing.

Over MCP this workflow needs at least `curator`: it can add ordinary active
memories, assert links, and flag contradictions, but cannot bless `core`. An
already configured operator host can perform those stronger decisions when
explicitly authorized; capability alone does not authorize them. Do not widen a
persistent curator mid-task. Profiles are fixed at startup. Ordinary CLI commands
route to the selected existing owner; they do not open a competing lease or bypass its capability. Use
that owner when it advertises the authorized operation. Offline `--db` is a
separate explicit path: if it or a different operator process is genuinely needed,
an operator can release/resume a quiescent lease; a curator cannot. Stopping a
curator is an operator-managed handoff, not a prerequisite for ordinary CLI saves.
Do not change services or profiles implicitly.

## 1. Save each item in the right owner

```
save{db:"project", summary:"<short, embeddable one-liner>",
     body:"<the full content>", tags:["topic", …],
     source:{namespace:"<origin>", key:"<stable source key>",
             reference:"<actual source pointer>"}}    # keep the returned id
```
CLI: `mnemed [--user] save "<summary>"` or `save --input FILE` (`-` for stdin).
The default kind is `note`; use `kind:"episode"` for an event. One shared Rust
policy owns admission, execution and verification; wrappers do not invent another
write policy. MCP needs explicit `db`, and SAVE admits only an existing current
store: no provisioning, upgrades, global fallback or competing lease.

Source is optional. When known, preserve actual observation/submission provenance;
supporting citations can also live in the body. Otherwise omit it and let native
preparation assign an honest manual operation identity, or supply a retained
`operation_id`. Never supply both. Exact retry repeats unchanged identity and
complete content, including links. Freeze the request before submission; a fresh
call omitting both identities is not a retry if the generated response was lost.
An ambiguous write is attempted/deferred, not saved. Read back the exact note or
episode edition in its owner store before claiming verified success.

Inspect the advertised installed catalogue/schema (or CLI help) first. If the
artifact lacks SAVE, deliberately choose the named sourced `capture` or
`episode action:"append"` compatibility route with its original verification
contract. Do not silently substitute these—or experimental raw ingest—for an
explicitly requested unsupported SAVE. If actual provenance is unavailable,
report the old-artifact limitation rather than fabricating a legacy source. This
source guidance is not a live-tool migration claim.

- **Scope it** with an explicit `db` and logical role. `user` is the global,
  private store for **user profile** (identity/preferences/habits) and separately
  **agent continuity/practice** (explicitly retained identity, working practice,
  cross-project lessons). Configured `project` owners hold **project knowledge**;
  explicitly device-enabled misc holds scoped ordinary work lessons/episodes,
  not personal/global identity. Known store presence alone grants no capture.
  Most saves remain scoped to their selected work owner; a
  project's memory must not depend on the user's. Do not automatically export
  private project content to the global store, even when a lesson seems reusable.
- **Ground it.** Record source, scope, and applicability; separate user statements,
  observations, and agent interpretations. For the Codex source-bearing SAVE
  workflow, agent-continuity items use namespace `codex-continuity`, a stable source
  key, and an actual source reference. Retry the same key and content after an
  ambiguous write, then read back the source and content. The namespace is not a
  store. Use the configured store/service; never trust an id without its db.
- **Status.** Ordinary notes are active and searchable immediately. Neither
  retrieval nor lack of use promotes, demotes, or archives them. Scope uncertain
  claims in their content and provenance; use explicit curation to archive or
  supersede obsolete claims. **Active is not core.** The `core` tag remains an
  operator-only, small always-loaded orientation choice, not a default for every
  useful memory. Do not re-ingest a fact to change its status; that duplicates it.
- **Note `stability` 0..1:** authored durability metadata, not truth, ranking, or
  search eligibility. Use a higher value only when its meaning is grounded.
- **The summary is what gets embedded** — make it a precise, searchable one-liner;
  put the detail in the body. Body-only details are not searchable. Note bodies
  cap at 256 KiB. CLI `save --body-file PATH` reads bounded UTF-8 content before
  checkout; use it instead of interpolating long text into shell arguments.

**Tag failures `pitfall`.** A gotcha, an anti-pattern, a hard-won "don't do X because
Y" → `tags:["pitfall", …]` (CLI `save --tags pitfall,topic`). That's what lets **mneme-adversarial** find it
later.

### Keep tags useful as filters

Tags are human-readable, broad, stable **reusable filters**, not miniature claims.
Prefer an existing broader topic/facet over inventing a tag for each note. Keep
note-specific distinctions in the summary/body: `directory-path-not-authority` is
a claim; `filesystem` is a useful shared topic. A shared tag does not merge
nodes or assert that different APIs mean the same thing.

Use the ordinary bounded overlap search and body-free `get` on nearby notes to
reuse meaningful facets; current query hits omit tags. Topology carries bounded
exact tag prefixes: check `tags_truncated`, not a supposed complete vocabulary.
A small `list` page can help; do not scan the whole store before each save. Partial
lookup does not establish absence, and similar names are not automatic synonyms.

Research runs, campaigns, waves and capture batches belong in source/provenance,
not tags. Omit a project-wide label that distinguishes nothing within its owner.
An episode is for an event worth remembering in its own right, never a dumping
ground invented to house batch metadata. Keep targeted API/person filters and
exact identifiers/casing when they genuinely help retrieval; new generic labels
should be readable lower-kebab-case. Chapter/topic facets are optional, not a
mandatory taxonomy. No tag count quota is implied.

For people, `people` groups bios/role context; an exact stable handle/name selects
a substantial subject; team/role tags such as `maintainers` or `api-review` provide separate
useful filters. An incidental mention needs no person tag. Keep display names,
dates and qualified role claims in prose: a tag does not prove current membership.
No extra `person:` or `team:` namespace is required.

Preserve special workflow/identity tags and their authority (`core`, `possibility`,
`pitfall`, and others with actual callers). Existing tags change only through an
explicit guarded retag after inspecting meanings and callers; authoring guidance
does not authorize automatic cleanup.

### Keep authored voice and history distinct

Write my own continuity in first person, not as a biography of "the assistant";
attribute the user's and other people's actions normally. Keep quotations and historical
source excerpts exact. A style correction does not make a memory false or justify
deleting/superseding it; use a supported in-place edit with its revision guard.

### Keep an interesting question open

A worthwhile idea, musing or unanswered question can be an ordinary note tagged
`possibility`. Preserve its hypothetical/open nature in the **summary itself**;
compact retrieval may omit tags. Keep the motivation and uncertainty, not an
invented recommendation or an assigned task. Do not turn every aside into memory.

`pursuing` means an explicit decision to act; research and enthusiasm are not that
decision, and a tag is not execution authority. `closed` means no longer open:
retain `possibility`, remove `pursuing`, and keep the original thought. Preserve
an answer/outcome/reason as ordinary knowledge or an episode, linked where useful.
Do not equate closure with archive, supersession or forgetting; do not combine
`pursuing` with `closed`. The conventions do not change engine ranking or learning.

Use ordinary SAVE, `list` with `tag:"possibility"`, and semantic query with the
same tag. The list includes closed items and may have empty advancing pages;
ranked search is not an inventory. Explicit tag changes use the generic `retag`
contract when advertised: read all current tags, retain unrelated tags, and supply
that exact expected set. Never resave or supersede merely to change a tag. See
[the operator guide](../../../docs/cli-and-mcp.md#open-possibilities-are-ordinary-notes).
Facts, distilled lessons and possibilities all use the same ordinary semantic
SAVE path; these tags do not introduce another node type or ranking policy.
Summary/body editing remains separate from tag editing. For guarded wording
corrections, use [mneme-reconcile](../mneme-reconcile/SKILL.md#in-place-editorial-corrections).

### Author personal meaning

The touchstone generation adds optional `touchstone` metadata to a normal
note SAVE: explicit subject plus same-store historical summary references obtained
from native GET. The working session authors what matters; this is not a human
approval queue or librarian-generated importance score. Name whose interpretation
it is. A changed interpretation is a new note, optionally superseding the old one.
Core membership stays a separate choice. A bare tag is not a typed reference.
See [touchstones](../../../docs/touchstones.md) for the exact workflow and coverage;
check the installed schema before using it. Never silently drop unsupported refs.

### Record a selected episode

Use SAVE's episode kind when the event itself matters:

```
save{db:"project", kind:"episode",
        source:{namespace:"codex", key:"<stable event key>", reference:"<actual source>"},
        summary:"<what happened, searchable without the body>",
        body:"<the scene, outcome, and what remained uncertain>",
        occurred:{kind:"point", at:<Unix epoch milliseconds>}, thread:"<optional thread>",
        occurrence_contexts:[{namespace:"<context namespace>", key:"<context identity>",
                              label:"<optional local display label>"}]}
```

Omit `occurred` when its time is unknown; a range uses `{kind:"range", start, end}`.
Do not invent precision from the session's recording date. Summary caps at 2 KiB,
body at 16 KiB. `links` uses the same bounded authored-link shape as notes; zero
is normal. A semantic lesson may later point to the concrete edition with
`derived_from`. Keep the observed episode and the reusable conclusion distinct.
No transcript import, journal backfill, automatic core tag, or compulsory lesson.

`occurrence_contexts` is optional and source-only; check the installed schema
before using it. Omission means unknown. Name where the event happened,
independently of its recorder; do not infer from source/session, host or CWD.
Namespace/key pairs are exact case-sensitive opaque identity, labels are local
display metadata. Text must be nonblank, trimmed and control-free; duplicate pairs
are invalid even with different labels. The nonempty collection is sorted by
namespace then key and bounded to 1024 canonical compact JSON UTF-8 bytes,
including labels and escaping—not a count cap on relevant memories. Never truncate
or silently discard it. `thread` stays an opaque historical label. Semantic notes
reject contexts. See [the contract](../../../docs/episodic-memory.md#where-it-happened-not-who-recorded-it).

SAVE returns stable `episode_id` plus concrete `edition_id`. Read back that
edition in the same store before claiming it saved. Use the native-backed bridge
when available for source/content verification. An ambiguous request stays
deferred; if retrying, repeat the frozen source or manual identity and payload,
not a new event. On an explicitly chosen older installed compatibility path,
`episode action:"append"` still needs its original source-keyed request.

An editorial correction uses `episode{db, action:"revise", episode_id,
expected_edition_id, reason, source, summary, …}`. Read the current edition first;
provide a new source key and the complete corrected account, not a partial patch.
Omitted occurrence contexts mean unknown, never inherited context; resupply the
whole collection if it still applies. It creates a new immutable edition, while
old citations still identify what was read. `history` lists editions. A later event belongs in a new episode; a changed
lesson uses the semantic successor/supersession path, not an edit to make the
past agree with the present. Revision needs operator capability; append needs
curator. Neither grants new store scope.

## 2. Wire associations

For a new SAVE, prefer its optional `links` array to a second
write: `[{to: "<same-store ULID>", kind: "associative", weight: 0.5}]`.
Read the target first and link only a useful relationship you can explain.
An associative link means “consider together,” not agreement: opposing claims can
be useful neighbours. A grounded analogy need not have high embedding similarity.
`associative`, `transition`, and `derived_from` are supported; at most eight
distinct existing targets, usually zero to three. The note and its links commit
atomically. Retrying the same request
never repairs deleted edges or overwrites learned weights. Links are part of the
frozen request, so changing them requires an explicit new operation rather
than reusing its identity. Check the installed schema advertises `save.links`
(or `capture.links` on the deliberately chosen compatibility path) before using
it. Links stay within one database; unsupported links must be omitted deliberately
or reported, never silently dropped.

For a relationship you already know is true, assert it directly:

```
link{db:"project", from:A, to:B, kind:"associative", weight:0.5}
```

Use the explicit source store every time; node ids are store-local. A user-memory
node may point to any other registered target through MCP `to_db:"NAME"`;
MCP requires source alias `user`. This is an explicit see-also reference, not an
edge ordinary query/context recall/walk follows. There is no current numeric
cross-store traversal penalty. Target presence grants no additional authority.

Do not use feedback as a synonym for "these facts relate." Grounded relevance
feedback comes from a completed read-only walk via `reflect{db, receipts, used}`.
The unreceipted MCP `feedback` compatibility tool is unavailable in curator mode. It
requires operator plus the deprecated `--allow-direct-feedback` startup spelling;
it is not an ordinary agent workflow and must never be added merely to make remember
more convenient.
That startup flag is MCP-only. The CLI has no profile/flag of its own; owner-routed
`mnemed feedback …` still needs the owner to advertise/admit it. Explicit offline
`--db` needs the exclusive lease. Do not widen a live host for convenience.

There is also no explicit **propose merge** MCP operation today. Existing merge
candidates can be reviewed with `merges`, but default MCP cannot turn an arbitrary
known-redundant pair into one. The CLI compatibility path
`mnemed feedback not-new --from A --to B` can bank that evidence; a receipt-bound or
explicit merge-proposal API is roadmap work, not something to fake with `link`.

## 3. Prefer asserted links over speculative consolidation

Installed front-ends disable random cross-cluster bridge minting by default because
it has not beaten the simpler grounded-feedback path. `reflect` trains the paths
actually walked; it normally creates no speculative long-range edges. When you know
two nodes relate, say so directly with `link`.

The consolidation hook remains an experimental engine/evaluation option.
The CLI `consolidate` route was retired because its production configuration
always disabled bridge minting. MCP `reflect` still invokes the hook under that
disabled default. Wire known relationships explicitly; do not use experimental
consolidation as the ordinary remembering workflow.

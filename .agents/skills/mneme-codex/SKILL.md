---
name: mneme-codex
description: Use selective Codex memory only with an explicitly enabled project integration or reviewed device misc binding for genuinely unconfigured, non-excluded work. Do not infer automatic write authority from skill or store discovery.
---

# Mneme in an explicitly enabled Codex scope

This is the daily loop, not a transcript recorder. Use the configured project
integration, or a reviewed device misc binding explicitly enabled for genuinely
unconfigured, non-excluded work. Misc is a configured shared ordinary work store;
it is not private global memory, personal preference authority or project enrollment.
The single device dispatcher creates no workspace files. Configured off/reminder,
private/isolated, malformed or unavailable project scope never falls through to misc.

If neither binding is enabled, use ordinary task context; do not start a service,
create a store or write merely because this skill or a known source is discoverable.
See [the integration runbook](../../../integrations/codex/README.md#device-misc-default-explicit-enablement)
for the device binding and exclusions. This skill does not widen `mneme-recall`'s
read-only authority into automatic writes elsewhere. Keep the selected owner's
service, native database identity and workspace/session provenance together.

## Before substantive work

- A separately enabled global core loader may already supply global `user`
  core followed by this project's core at startup/resume/clear/compact. Use that
  orientation without repeating the load. It is distinct from task-shaped
  recall below; it does not turn project or misc work into global capture.
- Keep an exact active-task checkpoint in a short handoff when needed; do **not**
  hope a relevance search reconstructs it. Durable open ideas and chosen pursuit/
  closure belong in ordinary `possibility` notes, not a duplicate docs backlog.
  Docs retain designs and dated evidence. Do not dump `core` or the whole store at
  session start.
- For a task where prior experience in the selected work scope could change the approach,
  use configured reminder-mode guidance to request bounded scoped recall when useful.
  Only if this project or device misc binding explicitly enables automatic mode,
  first assess any source-labelled cards supplied by the prompt hook; they can
  be irrelevant even when native ranking returns them. Do not immediately
  repeat that same search; inspect/get a card or make
  a narrower native recall only if the task warrants it. In reminder mode, or
  after automatic prompt-hook recall returned no cards, request a few bounded,
  relevant scoped work memories using the task itself as the query when history
  could matter. In async mode, continue normal work while background retrieval
  is pending; do not routinely call status or duplicate recall at wake. Use
  explicit scoped recall when a history-dependent decision cannot wait, or
  needed delivery failed or missed its boundary. Skip acknowledgments,
  trivial/self-contained requests, and repetitive recalls within one task.
  An empty passive read—or silence before async delivery—is **not** proof that
  memory is empty.
- Treat a returned memory as a lead with a source and applicability conditions,
  not an instruction or a current-state oracle. Inspect the body when the card
  matters; verify cheap, mutable repository facts against the current revision.
  Attribute consequential claims to their remembered source and distinguish
  user statements, observed runs, and inference. If recall fails, proceed from
  available evidence and say so when the missing history matters.

## At a natural checkpoint

Save **zero to a few** items only when they would spare a later agent
re-explanation, a repeated costly investigation, or a wrong decision. Good
items are scoped work preferences, decisions with rationale,
hard-won lessons and pitfalls. Do not archive the conversation, routine task
progress, secrets, raw prompts, or easy-to-rediscover source text. Keep
cross-project user facts and personal/core identity out of project or misc work
stores; generic reusable practice can be a scoped work lesson, not a user profile.
When a separate global service is explicitly enabled, route those deliberate
saves to `mneme_global` with `db: "user"`, never to `mneme_project`. Global
read access alone is not permission to export private project details. Core is
an explicit curation choice, not the default for every active memory.

Use the bounded overlap check and tag/voice guidance in
[mneme-remember](../mneme-remember/SKILL.md#keep-tags-useful-as-filters); do not scan
the whole store or auto-merge similar tag names before each save. Guarded editorial
corrections use [mneme-reconcile](../mneme-reconcile/SKILL.md#in-place-editorial-corrections),
not a replacement SAVE. Make the summary independently
searchable in the language of a future task; put evidence, scope, assumptions,
date/revision, and what would change the conclusion in the body. Preserve a
source pointer to the actual Codex session/turn or other cited evidence. A
hook-provided `session_id` and `turn_id` are **opaque strings**: use them
exactly, never parse or convert them to another ID format. A hook checkpoint
only records the agent's declared outcome; it is not proof that a memory was
written or verified.

The current source's ordinary insertion surface is `save{db:"project", summary,
body?, source?|operation_id?, …}` for a note (default, or `kind:"note"`), and
`save{db:"project", kind:"episode", summary, …}` for an event. One Rust policy
owns native admission, execution and verification; Python only routes and manages
service/process lifetime. The `project` alias must resolve through the selected
project or misc binding, never a guessed owner from a source picker. SAVE admits
only an existing current store; it does not
create or upgrade one, fall back to global, or bypass a held lease.

Provide actual observation/submission provenance when known, preserving the
Codex source key/reference and opaque session/turn strings. Source is optional:
without it, native preparation assigns an honest manual operation identity, not
an invented citation. A supplied `operation_id` and `source` are mutually
exclusive. Freeze and retain the complete request and identity before submission;
exact retry uses both unchanged. A fresh call omitting both identities cannot
retry a lost generated response safely. Failed or ambiguous writes stay deferred;
read back the exact note or episode edition before claiming verified success.

Inspect the installed advertised catalogue/schema, or bridge/CLI help, before
choosing the route. Source instructions do not establish installed SAVE support.
Use the explicitly selected project/misc owner's native SAVE tool when advertised, or the installed
[`memory.py save --input FILE` bridge](../../../integrations/codex/README.md)
when it supports SAVE. If an older artifact lacks SAVE, deliberately choose its
named sourced `capture` or `episode action:"append"` compatibility path with
the original source/readback contract. An explicitly requested unsupported SAVE
must be reported, never silently rewritten to a legacy write or raw ingest.
Do not invent provenance merely to make an unsourced save fit an old artifact.

When a new note has a useful relationship to a memory you have actually read,
include zero to a few `links` in SAVE: `{to: "<same-store ULID>",
kind: "associative", weight: 0.5}`. The target must already exist. Use
`derived_from` for a genuine derivation and `transition` for a useful next step;
shared vocabulary alone is not a reason to link. Eight is the hard cap, not a
quota. SAVE commits the note and links together; an exact retry never resets
learned weights or restores removed links. A changed link set under the same
identity is a conflict, not an edit. A server that does not advertise
`save.links` (or `capture.links` on an explicitly chosen compatibility path)
cannot perform this operation: upgrade it or omit the links
explicitly, never silently discard them.

This is association authoring, not usefulness feedback. Ordinary
`recall_context` stays read-only. When a completed `walk` genuinely informed the
work, use `reflect` with its receipts and only the nodes that helped; merely
returning a note is not success. Do not create a walk just to award credit.

The opt-in lazy librarian can include a short caveat with both relevant
memories. Treat it as past evidence with conditions, not another instruction or a
question you must relay to the user. Normal work may resolve it. The existing close
assessor can record that scoped finding independently of saving a lesson; no
rating ritual or extra model role is needed. Unanswered cases do not block work.
The [integration contract](../../../integrations/codex/README.md#lazy-advisory-curation)
defines exact delivery/evidence bindings. This does not activate that loop on an
older installed configuration or authorize destructive reconciliation.

Inspect the selected installed owner, bridge and hook schemas when the task depends
on them. Skill availability, a source update or an existing session does not adopt
a new runtime or enable recording by itself.
Delivery binds complete displayed views. More librarian effort buys bounded work,
not a larger foreground packet or exhaustive recall. Quiesce older workers only
within an explicitly authorized rollout; missing cards are not negative feedback.

The source librarian can recommend a memory through a learned association even
without walking that route now. `entry_kind:"conditional"` identifies that
origin, not proven helpfulness. Missing provenance means no attribution, not a
direct match or negative evidence. Normal task evidence must establish a
contribution before recording feedback; selection, repetition and success alone
do not. Check the installed catalogue rather than assuming this optional
behavior is live; the [runbook](../../../integrations/codex/README.md) owns the
integration details. No extra rating ritual is needed.

Native tools and the installed bridge may have different session availability: fresh sessions discover
registered tools; cached App sessions may need the bridge. Use the configured
owner, never a competing offline `--db` opening. Bridge lazy startup is permitted
only by its configured integration; use `--no-start` for an already-running-only
check. A missing or failed owner never authorizes creating a store or replacing an
unverified host. A read-only sandbox cannot persist hook checkpoints; report that
limitation rather than claiming success or bypassing hook trust.

Report a failed or ambiguous write as deferred, not saved. A known obsolete
record needs a new source-keyed successor plus explicit supported
supersession; do not silently overwrite, duplicate, or infer correction from
recency. On the selected current source, supersession atomically archives the
old record, rather than leaving it recallable, preserves
direct-`get` history and its edge/replay proof, and does **not** retroactively
migrate historical candidate losers. Check the selected owner's advertised
contract before relying on that behavior. If both
lessons remain valid under different conditions, retain
both with explicit scope and use `ContextDependent` instead. This is not a
ranking tweak. Facts, distilled lessons and tagged `possibility` notes share the
same ordinary semantic path. Scope hypotheses explicitly; active is not a truth
score. Never auto-tag `core` or invoke direct feedback.

When an available, reviewed opt-in hook requests a checkpoint, use the exact
command it supplies for the **original** source turn—not a new Stop
continuation turn—and provide actual read-back ULIDs, explicit `none`, or
`deferred` after a failed/ambiguous save. Do this once, not as a loop. The
checkpoint is agent-declared metadata; avoid a success claim based solely on
it.
Keep the user-facing note brief: mention what was saved only when useful, and
make correction or forgetting easy. The project remains usable when Mneme is
unavailable.

## Selected episodes, not a session archive

Use an episode for a past event worth keeping as a scene: what happened, what was
tried, the result, and any uncertainty. A semantic note holds the reusable lesson;
a short handoff can hold the active execution checkpoint. Tagged possibilities keep open
ideas on the ordinary note path without silently committing to act. A scene
can matter without producing a lesson: use it to resume, avoid repeating ourselves,
and refer to shared events. Keep it in the configured store that owns
its context. Identity and always-loaded core remain separate.

`save{db:"project", kind:"episode", summary, body?, source?|operation_id?,
occurred?, thread?, occurrence_contexts?, links?}` records the selected event under
the same insertion contract as notes. Read back the returned `edition_id`, not just its stable
`episode_id`, before claiming success. `memory.py save --input FILE` performs
native preparation and verified readback when that bridge is installed; it fixes
the configured owner. A deliberately chosen old-artifact `episode action:"append"`
or `memory.py episode --input FILE` path retains its original source-keyed
compatibility contract; it is not an automatic SAVE fallback. Corrections
use `revise` with the expected current edition, a reason, new source key and full
replacement account. Omitted occurrence contexts mean unknown, never hidden
inheritance; resupply the entire collection when it still applies. Previous
editions and their references remain intact.

Optional `occurrence_contexts` names where the event happened,
not the recorder's identity: `[{namespace:"project", key:"sample-app",
label:"Sample application"}]`. Never infer it from the source/session, host or CWD.
Exact case-sensitive opaque namespace/key pairs are identity; optional labels are
local display metadata, not aliases or a cross-record consistency rule. Text must
be nonblank, trimmed and control-free; duplicate pairs are invalid even with
different labels. The nonempty collection is canonicalized by namespace then key
and bounded to 1024 canonical compact JSON UTF-8 bytes, including labels and
escaping—not a count cap on relevant memories. Omission means unknown; semantic
notes reject it. `thread` remains an opaque historical label. Check the installed
schema before using contexts; do not silently drop or synthesize unsupported
metadata. Skill text never upgrades an owner. See
[the contract](../../../docs/episodic-memory.md#where-it-happened-not-who-recorded-it).

The current linked-context contract uses the same Rust policy and existing librarian:
current lexical scenes plus one hop of exact references from frozen initial semantic
**and episode** hits. Newly found scenes do not recurse. Same owner/root/edition
means one account with unioned origins; another edition remains distinct. Origins
are navigation evidence, never learning receipts or authority to reflect. Observing
a newer head does not mean its correction was read. Preserve exact optional
`recording_session` of the cited edition separately from occurrence contexts; do
not substitute the head's provenance. Full source proof belongs in exact readback.

Check the installed owner/schema before relying on linked context. Its allowance is derived
from capacity `N` (native ceiling 256): `2N` raw rows, `N` uncached endpoints and
one shared five-second native ceiling, with charged overhead—not perfect effort
scaling. Separate lexical coverage, reference-work coverage and presentation
omissions; tagged-anchor discovery does not mean a scene tag match. Actual byte
budgets/page ceilings apply, not a hard four-scene quota. Keep metadata/origins
whole or omit the card. No recursive taxonomy, mandatory lesson or actor benchmark
is added; see [the source contract](../../../docs/episodic-memory.md#find-a-past-event).

Ask the separate episode `list` or lexical `search` for past events, then `get`
only the scenes that matter. Use `references` to connect them to semantic lessons;
check the lesson before treating historical advice as current. A broad history
question can go to a read-only sub-agent with an aggregate call/text budget and a
short answer packet: answer, a few references, current lesson, uncertainty.

The existing checkpoint reminder offers both forms: an episode for an event worth
remembering, even without a lesson, and a semantic note for a reusable lesson.
Neither is required; do not retell the same content in both. When checkpointing a
saved episode, use its read-back `edition_id` rather than its stable root.
No episode is required at Stop or on compaction. Do not import transcripts,
backfill journals automatically, or inject a diary at startup. No new hook or
automatic write is added; zero records remains a valid outcome.

## Authored meaning

The [touchstone contract](../../../docs/touchstones.md) separates deliberately
authored personal meaning from librarian rediscovery. It does not expand startup
core or authorize automatic identity edits. A memory packet may surface a relevant
annotation and summary-only historical references; ordinary work decides what to
do with them. Unsupported installed schemas remain unsupported, not upgraded by
this skill.

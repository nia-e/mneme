---
name: mneme-recall
description: >-
  Read prior knowledge from mneme long-term memory and synthesize it into task
  context. Use only when the user explicitly asks for prior context, history,
  earlier decisions, or project memory (for example "what do we know", "have we
  seen this before", or "recall"), or when a concrete ambiguity has been identified
  that depends on a recorded prior decision. Do not invoke for every task merely
  because memory might help. Recall is read-only unless the user separately grants
  explicit memory-write authority for reflection or training. This focused card
  contains the ordinary workflow; consult the `mneme` hub only when exact lower-level
  mechanics are missing.
---

# mneme-recall — read memory into the task

Recall only for an explicit history/prior-context request or an identified ambiguity
that recorded context can resolve. Do not make recall a ritual at the start of every
task. When triggering on ambiguity, name the ambiguity and why a prior decision is
needed. This card contains the normal syntax. Do not load the full `mneme` hub by
default; consult it only for an exact transport, capability, storage, or walk detail
not answered here.

**Right-size the recall to the question.** Recall is not "dump the whole graph" —
match its depth to what's being asked:

- **Orientation / overview** ("tell me about this project", "who am I working
  with") → start with **`core{db}`** from the relevant authorized owners. The
  `core` tier is the curated direct-load compatibility list — it's often the whole answer. Stop there
  unless a specific gap remains. MCP returns `{nodes,total,truncated}`; read `nodes`,
  and report a curation warning if `truncated` is true; do not curate from this
  read-only workflow. The current `core` tag is not isolated from ordinary retrieval
  and the direct listing is not a proper deterministic tier yet.
- **A specific prior-context question or identified ambiguity** → start with bounded
  typed context from the selected authorized owners:
  `recall_context{db, text, k:4, max_nodes:8, depth:1}` (CLI:
  `mnemed [--user] recall-context "…" --k 4 --max-nodes 8 --depth 1`). Stop if it
  answers the need. Inspect the actual client/owner schema. Ordinary lexical
  episodic admission remains current-summary lexical. The current linked-context
  contract additionally follows one exact-reference hop from frozen initial semantic
  **and current episode** hits in shared Rust and the same librarian; newly found
  scenes never recurse. Keep the account's exact edition and coordinates rather
  than treating a past scene as a current rule.
  The escalation ladder is **`recall_context` → `query` →
  `recall` → `walk`**: raw seeds, then bounded one-hop context, then manual graph
  navigation. Climb only until the missing evidence appears; keep it proportional,
  not a walk per cluster.

- **What happened / how a decision changed** → use the episodic lane, not a
  broader semantic dump. Start with `episode{db, action:"list", limit:8}` for a
  timeline or `episode{db, action:"search", cue:"…", limit:8}` for a named event.
  Search matches current-edition summaries lexically. Narrow with `thread` or
  occurrence bounds when known; recording date is not necessarily event date.
  Read a selected scene with `episode{db, action:"get", episode_id, body:true}`.
  For a referenced historical account, supply its exact `edition_id`; root-only
  `get` reads the current head.
  `episode{db, action:"references", anchor:<edition-or-lesson-id>, limit:8}`
  exposes links both ways; use it to find the lesson anchored in an event, or the
  events behind a lesson. Read the lesson before calling it current. Old scenes
  may accurately describe a conclusion that was later replaced.

In that linked-context view, one owner/root/edition has one card with unioned lexical
and typed reference origins; another edition is another account. Reference origins
retain anchor type, stored direction/kind and any body span on `from`, not semantic
learning receipts. A newer observed head is not a read correction. Exact optional
`recording_session` belongs to the cited edition and is capture provenance, not
occurrence context; human source labels are display previews, not full proof.
Separate lexical coverage, reference discovery/read-work coverage and presentation
omissions. An indirect scene from a tagged anchor did not itself match the tag.

The capacity-derived linked allowance is `2N` raw rows and `N` uncached endpoints
(`N` native ceiling 256), with charged overhead and a shared five-second native
ceiling—not perfect effort scaling or completeness. Real shared byte/page limits
apply, not a hard four-scene quota; metadata and origins survive whole or the card
is omitted. Check the installed owner/schema before relying on linked context;
source qualification is not a deployment claim or an episodic actor benchmark.
Recall grants no memory writes, training or reflection. See
[the contract](../../../docs/episodic-memory.md#find-a-past-event).

For a broad episodic question, a read-only sub-agent can spare the main context a
pile of scenes. Give it the question, exact store, relevant time/thread scope,
and an aggregate budget (for example 12 calls and 8 KiB of returned material).
Ask for a short **answer, a few episode/edition references, the current lesson
if one exists, and remaining uncertainty**. It should inspect only enough to
answer, never reflect or write. An exact event lookup needs no delegation. Do not
turn a history request into importing journals or reading every past session.

### Dormant personal meaning

The [touchstone view](../../../docs/touchstones.md) adds authored meaning
and historical summary references to ordinary bounded context. A scene may nominate
its annotation; an annotation may supply cited summaries. Neither is an instruction
or a relevance guarantee. Keep the named subject, exact store/edition and current
resolution distinct. Reference omissions are not read evidence. Deliberate browsing
uses `list{db,kind:"touchstones"}` / `mnemed list --touchstones` with continuation,
not a top-k query. Inspect the installed schema; do not widen project-only scope.

## 1. Find entry points when bounded context is insufficient

```
query{db:"project", text:"<a sentence or two describing the task/context>"}
```
Use the actual selected alias; query `user` separately only when global context is
relevant and authorized. CLI: `mnemed [--user] query "…" --json` uses its selected
owner. Start with a few useful seeds (for example **2–4 total**) across relevant
owners, and retain the
service and `db` each came from (you walk it there). Add
`archived:true` is for explicit historical inspection, not ordinary recall. All
nonarchived semantic memories are searchable immediately; a read earns no credit.

Before opening sessions, try `recall{db, text, expand_top:3, neighbors_each:5}` for
one bounded associative hop. It is still read-only and often supplies the missing
relationship without the session/cursor choices of a walk.

## 2. Walk selected seeds (read-only)

A budgeted traversal: read the current node, step along an edge, gather a trail.
The walk is **read-only** — it browses and **trains nothing**, so explore freely
without skewing the graph. `query`/`recall` also write no exposure, co-retrieval
topology, or checkpoint. Drive walks locally by default:

- `walk{action:"start", db, start:<seed>, budget:12}` → keep the `session`. Then
  call `walk{action:"body"|"edges", session}` to inspect and
  `walk{action:"go", session, to:<index-or-id>}` / `back` to move **freely** (no
  signalling). `done` returns the visible **trail** plus an opaque, single-use
  **receipt**. Finish with `abort` by default. Use `done` and retain its receipt only
  when explicit memory-write authority permits a later reflection. Body reads
  default to 64 KiB and return `source_start`, `source_end`, `next_offset`, and
  `has_more`; continue with `body_offset:next_offset` only when the missing bytes
  matter. `body_truncated` is not the range cursor.
- **Optional parallelism:** use sub-agents only when the host permits them, multiple
  independent seeds justify the coordination cost, and parallelism materially helps.
  Keep every delegated walk read-only; no sub-agent may reflect. A suitable brief is:

  > Explore an associative memory graph from a start node for anything relevant to:
  > **\<task context\>**. `walk{action:"start", db:"<DB>", start:"<SEED>", budget:12}`,
  > keep the `session`. Then use `walk{action:"body"|"edges", session}` and
  > `walk{action:"go", session, to:<index-or-id>}` / `back` (read-only browse —
  > move freely, no signalling). Follow relevant material, retreat from dead ends,
  > `abort` when the budget's spent and return the **`trail`** verbatim plus a 1–2
  > sentence note on which nodes were actually useful. Only use `done` and return a
  > receipt when the coordinator states that explicit memory-write authority exists.

## 3. Synthesize

Collect the trails, dedupe, and pull bodies for the few that matter:
`get{db, id, body:true, edges:true}`. Synthesize **at the altitude of the question** —
an overview shouldn't recite low-level dev gotchas. **The graph is your recall
source**: don't re-read the filesystem to re-derive what you just recalled (only hit
disk to verify a specific live detail). A `get` body range reports its cursor under
`body_range.next_offset` with `body_range.has_more`; continue by passing that offset
as `body_offset`.

## 4. Optionally reflect under explicit memory-write authority

Do not call `reflect` unless the user explicitly authorized memory writes or training
for this task. A request to recall, inspect, answer, edit a repository, or complete a
task is not that authority. Without it, abort walks or discard receipts and stop.

When authority exists, wait until the answer exists so training is grounded in what
actually fed it rather than in-the-moment guesses:

```
reflect{db, receipts:[<receipt from walk done>], used:[<ids that informed the answer>], unhelpful:[<explicitly unhelpful ids>]}
```

`reflect` applies grounded path feedback:

- **Reinforce / interfere (receipt routes).** `used` earns positive credit; optional
  `unhelpful` banks interference. Omitted nodes are unknown and stay unchanged.
  Feedback follows the exact stored arrows to judged nodes. Duplicate arrows train
  once per batch; conflicting judgments on one arrow leave that edge unchanged but
  preserve each node's judgment. Incoming hops stay incoming; a missing endpoint/edge
  or no-longer-traversable direction skips edge training instead of recreating or
  reversing it. A judged start with no route receives node-only feedback.
  (CLI: inside the REPL, `done <used-id…>` gives positive-only feedback;
  bare `done` trains nothing.)

Group receipts and judged ids **by database**, then make at most one reflect call per
db (up to 8 receipts and 64 total ids across `used` and `unhelpful`). The two sets must
be disjoint, and every judged id must have been visited by one of those receipts.
Receiptless reflection is refused in every profile. Genuine positive walk feedback
also invokes a consolidation hook, but defaults disable speculative bridge creation;
do not expect reflect to invent long-range links. Skipping reflect performs no
relevance training—safe. One local-store receipt batch is atomic and exact-retry idempotent only
inside the issuing MCP host epoch under its exclusive database lease; restart kills
the receipt capability. Just **don't mark everything used**: that's the broad-query
tell.

Then use **mneme-adversarial** only when its explicit or consequential-decision
trigger applies. Use **mneme-remember** only when the user has authorized a durable
memory write.

---
name: mneme
description: >-
  Shared reference mechanics for Mneme's CLI/MCP surfaces, owner scopes, capability
  profiles, bounded retrieval, and constrained walk protocol. Use when wiring a
  Mneme integration, checking exact tool/limit/lease behavior, or when a focused
  Mneme workflow explicitly routes here. Do not select this hub for an ordinary
  recall, write, curation, or adversarial loop; use the focused skill instead.
---

# mneme — agentic memory (the hub)

mneme is a memory **graph**: nodes are summaries pointing at bodies; weighted,
directional edges are associations; retrieval fuses sparse and dense entry points
before an optional bounded graph leg. Its **memory policy is LLM-free** — the
engine does not decide what is relevant or worth remembering. *You* do. Semantic
retrieval still uses a fingerprinted embedder and may use an optional reranker.

This skill is the **shared reference**, not an agent loop. The workflows live in
focused skills:

- **mneme-recall** — pull prior knowledge into a task before working.
- **mneme-remember** — save durable notes or selected episodes after.
- **mneme-reconcile** — resolve contradictions, merge redundancies, maintain.
- **mneme-adversarial** — surface counter-evidence before committing to a plan.

Semantic notes hold reusable knowledge. **Episodes** hold selected things that
happened, with their own timeline and cue search. Facts, distilled lessons and
tagged open possibilities share the ordinary semantic note path. Both notes and
episodes live in the same graph;
an explicit link can anchor a lesson to an episode without mixing their recall
or maintenance policies. An episode does not need to teach a lesson.

## How to drive it: CLI or MCP connector

Same graph either way; use the front-end you have.

- **`mnemed` CLI** — when the binary is on PATH. Install: `cargo install --path
  crates/mnemed` (cozo + fastembed are the defaults; `--no-default-features` is the
  dependency-free reference build). Ordinary commands support `--json`; TUI is interactive.
- **MCP connector** — in a sandbox/chat with the mneme MCP server hooked up (no
  binary/PATH). Call its tools directly. Tools take a **`db`** argument — a logical
  name from the registry, typically `user` and `project` (`databases{}` lists them).
  Omitted read `db` selects registered `project`, else the sole registered store;
  multiple names without `project` fail as ambiguous. A released/unavailable
  project never falls back to user. Every mutation requires explicit `db`.

**Surface differences are listed below. The last column is the
minimum process-wide MCP capability profile. Owner-routed CLI commands respect
that profile too; explicit offline `--db` has separate local authority:**

| operation | `mnemed` CLI | MCP tool | minimum profile |
|---|---|---|---|
| bounded model context | `recall-context "…"` | `recall_context{db, text, k?, depth?, max_nodes?, tags?}` | `read-only` |
| find seeds | `query "…" [--tag t] [--archived]` | `query{db, text, tags?, archived?}` | `read-only` |
| **recall + expand** (the everyday diagnostic) | — | `recall{db, text, expand_top?, neighbors_each?}` | `read-only` |
| read a node / its edges | `get <id> --body --edges` · `neighbors <id> [--limit N] [--after CURSOR]` | `get{db, id, body?, edges?}` · `neighbors{db, id, limit?, after?}` | `read-only` |
| paginated canonical inventory | `list [--status active\|archived] [--tag t] [--after CURSOR]` | `list{db, kind:"nodes", status?, tag?, limit?, after?}` | `read-only` |
| lightweight graph / lazy summaries | TUI (selected owner) | `graph{db, action:"topology", limit?, after?}` · `graph{db, action:"summaries", ids}` | `read-only` |
| known sources (metadata only) | `stores [--json]` · TUI Tab picker | — (shared client projection, not an owner tool) | — |
| core set / status | `core` · `status` | `core{db}` · `status{db}` | `read-only` |
| episodic timeline / cue / exact read | `episode list` · `episode search "…"` · `episode get <episode-id>` | `episode{db, action:"list"\|"search"\|"get"\|"history"\|"references", …}` | `read-only` |
| save a selected event | `save --kind episode "…"` · `save --input FILE` | `save{db, kind:"episode", summary, source?\|operation_id?, …}` | `curator` |
| correct an episode's account | `episode revise <episode-id> --input FILE` | `episode{db, action:"revise", episode_id, expected_edition_id, reason, source, summary, …}` | `operator` |
| save an ordinary note (default kind) | `save "…"` · `save --input FILE` | `save{db, summary, source?\|operation_id?, body?, tags?, stability?, links?}` | `curator` |
| replace inspected tags | `retag ID --expected-tags … --tags …` | `retag{db, expected_db_id, id, expected_tags, tags}` | `curator` (`operator` if either set contains `core`) |
| edit body without changing identity | `edit-body ID --expected-body-revision REVISION --body-file FILE` | `edit_body{db, expected_db_id, id, expected_body_revision, body}` | `operator` |
| edit summary and refresh search | `edit-summary ID --expected-snapshot-sha256 HASH --summary "…"` | `edit_summary{db, expected_db_id, id, expected_snapshot_sha256, summary}` | `operator` |
| named Codex sourced-write compatibility | `capture add --input FILE` · `episode append --input FILE` | `capture{db, source, …}` · `episode{db, action:"append", source, …}` | `curator` |
| experimental import (current Mindcraft caller) | `ingest --summary … --body … --tag …` | `ingest{db, summary, body?, tags?, stability?}` | `curator` |
| save a core note, explicitly authorized | `save --input FILE` with `tags:["core"]` | `save{db, tags:["core"], …}` | `operator` |
| direct feedback compatibility | `feedback <signal> --from A --to B` | `feedback{db, from, to, signal}` | `operator` plus startup opt-in |
| assert edge / flag contradiction | `link …` · `contradict …` | `link{…}` · `contradict{db, a, b}` | `curator` |
| **constrained walk** (read-only) | `repl <start> --budget N` | `walk{db, action, …}` | `read-only` |
| **train on a walk** | `repl …`, then `done <used-id…>` | `reflect{db, receipts, used, unhelpful?}` | `receipt-grounded` |
| inspect merges / contradictions | `merges` · `reconcile` (list) | `merges{db}` · `contradictions{db}` | `read-only` |
| inspect advisory cases (source prototype) | `concern --input FILE` | `concern{db, action:"list", endpoint, after?, limit?}` | `read-only` |
| notice / record scoped finding (source prototype) | `concern --input FILE` | `concern{db, expected_db_id, action:"notice"\|"record_finding", …}` | `curator` |
| adjudicate | `merge …` · `supersede …` · `reconcile … --as …` | `merge{…}` · `supersede{…}` · `reconcile{…}` | `operator` |
| maintenance / forget | `decay` · `prune` · `forget` | matching tools | `operator` |
| lease status | — | `database_control{db, action:"status"}` | `read-only` |
| release / resume lease | — | `database_control{db, action:"release"\|"resume"}` | `operator` |

Editing requires the selected owner's advertised contract, not just a recent CLI.
Use GET's exact expected tags/body revision/summary snapshot; inspect stale or
ambiguous outcomes rather than blindly replaying. Episode and touchstone-owner
guards preserve authored history. See [in-place editing](../../../docs/cli-and-mcp.md#edit-tags-in-place).

Ordinary CLI prefers the current project's `.mneme/cli.json` owner. Only genuine
absence of project configuration may select one device `$XDG_CONFIG_HOME/mneme/misc.json`
(`~/.config/mneme/misc.json` by default), using the same closed `mneme.cli.owner.v1`
schema and the ordinary `project` alias. Only this misc record accepts optional
bounded `excluded_roots`; default fallback checks lexical/canonical ancestry
before owner contact. Project/user records reject the field. This is shared work
memory, **not private `user` memory**. Configured/off/reminder/private/isolated scopes and
owner failures do not become unconfigured work. `--user` selects its separate
`cli.json`, never misc. `--remote` overrides enrollment; `--db` / `MNEME_DB`
explicitly selects an offline path. Nothing creates workspace enrollment files.
TUI and REPL use the selected owner; TUI Tab and `stores` inspect known sources,
not network discovery or auto-enrollment. Explicit picker selection is a deliberate
read-only override like `--user`/`--remote`, not permission for automatic capture.
`tui --config PATH` is explicit library
mode, `--demo` synthetic. CLI misc enrollment alone is not recorder enablement:
Codex requires its reviewed device misc binding. New source contracts are not an
installed-runtime claim. See [selection and guards](../../../docs/remote-cli.md).

General list defaults to `kind:"nodes"`, all statuses, 50 cards (maximum 64),
canonical ID ascending—not creation-time or relevance order. It includes exact
historical episode editions. Continue with native `next_cursor` and unchanged
status/tag filters; an empty filtered page may still have more. Output, summary
and physical scan work are bounded. Cursors bind database identity and the
initial upper key, not an atomic snapshot. `kind:"touchstones"` retains its
separate indexed collection and existing bounds. Neighbor pages cap at 64 raw
incident edges, including episode/missing endpoints, with `after` continuation;
these are inspection, not the walk's legal-move set. `graph` topology pages
contain IDs, lifecycle status, exact authored tag prefixes (at most 256 JSON
bytes per node) and edges (default/max 256 records). Check `tags_truncated`; summaries and bodies stay lazy.
`graph` summary batches take 1–64 unique exact IDs, marking missing slots and
summary/tag truncation. Both bound response bytes and work, read no bodies, and invoke no inference or learning.

**Offline-only:** provisioning, `reindex`/`reembed` and migrations/upgrades require
explicit `--db PATH`. Repository bootstrap and libraries
retain their separate authority. Native owner calls expose `expected_db_id`
checkout guards; all scoped implicit owner calls, including reads, require them.
Older owners fail with an update action, not preflight-only routing. Check installed
schemas first. Explicit `--remote` alone is not a pinned enrollment identity.
The callerless standalone `communities` CLI diagnostic is retired. Use TUI browsing
or `reconcile` conflict inspection for current workflows; neither promises that
same clustering algorithm, and engine community policy is retained.

The routed MCP `recall_context` can use `routing_hints` to nominate a
target from historical experience. Its `conditional_binding` is distinct from a
graph path: read-only admission is neither feedback nor a relevance certificate.
Ordinary CLI recall has no hint flag; plain CLI/MCP behavior remains unchanged.
The [integration runbook](../../../integrations/codex/README.md) describes this
automatic path; it adds no step to everyday manual recall.

### Advisory cases: concern

Available on owners with ConcernV1 or its successors. Check the installed
catalogue before using it. A case binds two same-store node meanings and retains
one latest scoped finding; it does not merge, supersede or decide which node is
universally true. Use native GET's `concern_endpoint`, never reconstruct its hash.
For a finding, retain the complete native row as the expected value. Mutations
require the observed database identity; stale expectations are refused, not
silently refreshed. Native atomic results are authoritative—no substitute GET.

Endpoint lists have a real cursor. Omit `limit` for the byte-derived wire chunk;
follow `page.next` while relevant and within the work allowance. A partial page
sequence is not an inventory or evidence of absence. External evidence digests
identify bytes historically inspected, not current file contents. See the
[integration contract](../../../integrations/codex/README.md#lazy-advisory-curation)
and [curation workflow](../mneme-reconcile/SKILL.md).

### Ordinary insertion: SAVE

The current source uses `save` for a note (default, or `kind:"note"`) or an
immutable episode append (`kind:"episode"`), with one Rust admission/execution/
verification policy shared by CLI, MCP and the Codex bridge. MCP requires an
explicit `db`; SAVE admits only an existing current store. It does not create or
upgrade a database, resolve lease contention, or fall back to global storage.

`source` is optional: provide actual observation/submission provenance when known,
not a fabricated citation. Without `source`, native preparation gives the manual
submission an operation identity. `source` and `operation_id` are mutually
exclusive. Exact retry requires the unchanged identity **and complete content**,
including links; freeze and retain them before submission. A fresh call omitting
both identities is a new operation, not a safe retry after a lost response. Keep
an ambiguous write as attempted/deferred; read back the exact note or episode
edition before claiming verified success.

Inspect the installed tool catalogue/schema (or CLI help) before choosing a write
route. These source instructions do **not** prove the live tools migrated. If an
older artifact lacks SAVE, deliberately choose its named `capture` or
`episode action:"append"` compatibility path for a sourced write, preserving that
path's source/readback contract. If SAVE was specifically requested but is
unsupported, report that gap; do not silently translate it to capture, append or
raw ingest. Unsourced SAVE cannot be faked by inventing a legacy source.

`decay` and `prune` use bounded keyset pages and conditional row mutations.
`decay` affects edges, not node admission. The engine
releases the mutation gate between chunks and reports conflicts for the next
pass. This is online-safe with the write-through Cozo backend. Snapshot-backed
hosts still perform a final O(N) checkpoint, so treat their maintenance as
offline. Community
consolidation, re-embedding/migration, and snapshot replacement remain explicit
offline work.

MCP defaults to `receipt-grounded`: read-only tools plus `reflect` with at least one
server-issued walk receipt. Receiptless `reflect` is refused in every profile.
`curator` adds ordinary non-`core` SAVE (and compatibility writes), explicit
links, and contradiction flagging. `operator` adds core curation, destructive
adjudication, maintenance, and lease control. Tool discovery and dispatch both
enforce the profile, but this is ambient-authority reduction—not authentication,
tenant isolation, or protection from a same-UID process. The deprecated
`--allow-direct-feedback` spelling selects/needs operator and is the only way to
expose unreceipted `feedback`; never add it to a persistent curator deployment.
Changing profile requires a restart. Use the selected owner for ordinary CLI/MCP
operations. A separate offline `--db` or operator process needs an explicitly
authorized, quiescent handoff; a curator cannot release/elevate in place. Do not
change a long-lived host or stop services implicitly for a curation task.

The unsafe legacy partial-merge writer has been removed from the engine and both
frontends; persisted `Partial` verdicts remain readable for snapshot compatibility.
A replacement requires an atomic child-plus-derivations contract.

**Recall tiers.** Use **`recall_context`** for ordinary model-facing context: it packs
typed primary and episode summaries under one exact 32 KiB envelope and emits no
feedback receipt. Use diagnostic `query` (raw seeds), `recall` (seeds plus one-hop
inspection), and finally **`walk`** when provenance or graph navigation matters.
Reach down only when the cheaper result comes up short.

**Episodic recall is a separate lane within ordinary context.** `recall_context`
automatically includes bounded lexical matches as typed `episodes`, sharing its
existing output budget with semantic notes. Library context does the same across
its selected sources. Keep edition, event identity, timestamps and source with
each card; an episode is a dated account, not automatically a current conclusion.
Inspect separate lexical and reference coverage: tags skip episodic lexical
search, but linked context can still discover exact-reference scenes from tagged
semantic anchors. Those scenes did not themselves match the tag. Old stores can
report the lane unavailable; empty results do not establish an empty history.

Use `episode{db, action:"list", limit:8}`
for a bounded timeline, or `episode{db, action:"search", cue:"…", limit:8}` for
lexical search of current-edition summaries. Search is not semantic ranking and
does not expand the graph. `episode{db, action:"get", episode_id, body:true}` reads
the current account; add `edition_id` to read a particular historical edition.
`history` lists editions; `references` takes an `anchor` node ID and reads explicit
incident links in both directions. Raw semantic query/ranking, walks, core loading,
and automatic maintenance exclude episodes. Ordinary exact `get` and explicit
links remain available across the two kinds.

The stable `episode_id` identifies the event; `edition_id` identifies the account
being cited. An edit creates a new immutable edition with a reason, new source key
and expected current edition. It does not create a second event or retarget old
references. Capture times and occurrence times are distinct; unknown occurrence
is valid. Times use Unix epoch milliseconds. Pages default to eight and cap at 32;
follow a returned `next` only while the question needs more. These pages are not
snapshots across concurrent edits. Keep store identity with every ID.

**MCP transport.** Stdio is newline-delimited JSON-RPC. The HTTP server implements
stable MCP `2025-11-25` Streamable HTTP (and negotiates the two older supported
revisions). Send `initialize` as one request with no session header; retain the
returned `mcp-session-id` for later requests. HTTP POST requires
`Content-Type: application/json` and an `Accept` value containing both
`application/json` and `text/event-stream`; batches are rejected. Send the
negotiated `mcp-protocol-version` on later requests; if supplied it must match the
session. `DELETE` closes a session; GET streaming is not implemented. Browser
requests need the one exact configured Origin, and non-loopback binds require a
bearer token plus TLS in front.

**Service limits.** MCP rejects caller-supplied query `k` outside 1–64,
`max_nodes` outside 1–256, or depth outside 0–12 before database checkout; trusted
configured defaults are bounded separately. Recall expansion caps at 16×16 and walks
at 64 distinct nodes. Explicit body-range
reads default to 64 KiB, cap at 1 MiB, and expose `next_offset` plus `has_more`; use
that cursor to continue. `body_truncated` is retained only for aggregate/compatibility
prefix responses and is not the range protocol.
HTTP and stdio each cap one request at 2 MiB; an ingested body caps at 256 KiB.
`core` returns at most 32 nodes under a shared 128 KiB body allowance. `recall_context`
uses an exact 32 KiB typed envelope with bodies absent. Tool text is
replaced above 256 KiB and the exact serialized JSON-RPC frame is replaced above 512
KiB on both transports. The context envelope is a real per-call presentation bound;
the other fuses are post-work, and none is an aggregate cross-call/task budget.
Repeated calls accumulate. Still stop when you have enough evidence.

**Run the cold path.** Grounded use and edge feedback remain; `decay` spends
pending **edge** interference, and `prune` GCs speculative/excess edges. Neither
operation automatically demotes or archives nodes. `decay` can settle pending edge
interference at an authorized checkpoint. `prune` is a separate explicit maintenance
choice, not an everyday save/recall checkpoint or automatic insertion/autolink step;
the current lazy-librarian prototype does not authorize destructive density pruning.
`origin_commit` records the host's git HEAD at insertion when
available. CLI `get` shows the full hash (or `(none)`) in human output; CLI JSON and
MCP `get` expose `origin_commit` as the full hash or null. This records ingestion
context, not the claim's validity interval or proof that it came from that revision.
Keep authored provenance and applicability conditions explicit. CLI JSON and MCP
`get` also expose `created`, `last_exposed`, and `last_grounded_use`.

**fastembed matters** for a CLI store you build: real embeddings recall by *meaning*,
not token overlap. Don't switch embedders mid-store (lexical and semantic vectors
aren't comparable). Stores persist the full embedding fingerprint and fail closed
on a legacy/mismatched index; run `mnemed --db PATH reembed` for an intentional offline migration.

**Concurrency and ownership.** One MCP server holds an exclusive OS lease on every
open database. Requests through that same server may overlap;
inference is process-shared, limited to one executor, and admits at most four calls
with a five-second execution-wait deadline. HTTP sheds above 32 in-flight POSTs before
body aggregation. Learning read-modify-write sections are serialized. `mnemed` also
routes ordinary commands through the selected owner without opening its database.
Explicit offline `--db` commands take an exclusive lease and fail fast if another
process owns that store. Never open two owning processes on the same live database.
For authorized offline CLI work, use an explicit operator deployment and `release`
through `database_control` only
after walks and unconsumed or claimed receipts are gone, run the command, then
`resume`; normal MCP tools fail closed during the handoff. Release also refuses while
detached backend jobs outlive a cancelled request, rotates only that database's
volatile feedback epoch, and purges the consumed retry tombstones made unreachable
by that rotation. There is deliberately no timer eviction: request-idle is not proof
that walk/receipt/backend state is quiescent.

If an explicitly authorized offline handoff needs a curator-owned store, the
operator must stop that curator because it cannot call operator-only `release`.
This is not needed for owner-routed ordinary CLI commands and does not authorize
service changes. A competing operator cannot acquire the curator's held lease.

## Selected owners and logical roles

Scope belongs to the selected owner binding, not a topic or registry alias:

| Logical role | Physical store | What belongs here |
|---|---|---|
| User profile | `user` | User identity, preferences, and cross-project habits |
| Agent continuity / practice | `user` | Explicitly retained agent identity, working practice, and reusable cross-project lessons |
| Project knowledge | configured `project` owner | This project's facts, decisions, constraints, and experience |
| Unconfigured work | shared misc owner (`project` alias) | Scoped ordinary work lessons and episodes, retaining actual workspace/session provenance |

The **user store** is global and private (`~/.local/share/mneme/memory.db`), never
shipped with a project. The **project store** lives under the repo's gitignored
`.mneme/`; the Codex integration uses `.mneme/codex-memory.db`. Misc is a separately
configured existing ordinary work store, not private global memory or
implicit enrollment of every workspace. Keep each node's role, scope, source, and
applicability explicit. A lesson learned in a project does not become global merely
because it sounds reusable: do not automatically copy private project material to
the user store. Global read permission is not blanket capture or publication
permission.

The explicit offline project resolver requires the conventional `.mneme` parent to be a real
directory. A symlink fails closed before any child is inspected or created; do not
work around that guard with a repository-controlled redirect.

Recall may read selected owners within the authorized scope; remember routes by role
and scope (most saves are project-scoped). Node ids are **per-store**, so retain
the `db` and service identity with every id. A walk or `get` must target the same
store. A configured global-only session must not discover or open other projects.

**Cross-db see-also edges.** MCP requires the source registry alias `user` and
allows any other registered target, not only project-role stores. For example:
`link{db:"user", from:<userId>, to_db:"project", to:<targetId>}`, or CLI
`mnemed --user link --from USER_ID --to TARGET_ID --to-remote-db NAME`.
The reference retains the target's native database/node identity. Explicit
`get edges:true` or `remote_edges` can inspect it; unavailable targets remain
unresolved rather than resolving a replacement under the same name.

These references are **not followed by ordinary query, context recall or walks**,
nor drawn as cross-source TUI links. There is no current numeric traversal penalty
or automatic learning loop for them. A known target grants no read/write authority.
The user-source restriction is a frontend authoring rule, not a persisted role
invariant for every offline store.

## Core memory — direct-load orientation, separate from task recall

A small, explicitly curated set tagged **`core`** is the original direct-load
orientation mechanism, not a relevance-ranked search tier. It can contain user
profile, agent continuity/practice, and current-project orientation. Under the
user-authorized global Codex workflow, load it at **startup, resume, clear, and
compact**: **global `user` core first, current allowlisted `project` core second**.
Outside an allowlisted project, load only global core. Do not search other projects
or promote their content into global memory to fill a gap.

```sh
mnemed --user core    # CLI          core{db:"user"}      # MCP
mnemed        core    #              core{db:"project"}
```

Use the configured owner through MCP, its bridge or ordinary owner-routed CLI;
never open its live store through a competing offline `--db` process. The Codex
passive core loader waits boundedly for the MCP launcher-owned host and warns with a manual fallback if unavailable; it never
starts a host, initializes a store, queries by topic, or captures memory. In the
Mneme checkout, [the integration guide](../../../integrations/codex/README.md)
describes configuration.
Installed skill availability alone does not prove the host or hook is configured.

Each direct load returns core nodes **with bodies**. This happens once per context
boundary, not on every prompt. Task-shaped recall remains a separate decision:
use the focused recall skill when prior knowledge would help. Core nodes currently
remain eligible for ordinary ANN/BM25, graph expansion, reranking,
and deduplication; direct loading is a workflow contract, not a search exclusion.

Bless something as core only under explicit operator authority. **Active does not
mean core**, and useful does not mean always loaded. Core nodes never decay: keep
the set small, grounded, and curated; retire a node when it no longer applies.
Do not re-ingest a memory merely to “upgrade” it: that creates a second node.
Ordinary memories are active on creation; `core` remains a separate operator
choice. Source-bearing agent continuity saves use source namespace
`codex-continuity`, with a stable source key and an actual source reference; the
namespace records provenance, not a store or permission to capture everything.
The MCP response is bounded and wrapped as `{nodes,total,truncated}`; read `nodes`
and treat `truncated:true` as a curation warning, not an invitation to inflate core.
Bodies also share one 128 KiB allowance across the response.

## The walk protocol (constrained, read-only traversal) + reflect

A stateful, sandboxed walk that offers only legal moves — safe to drive directly or
hand a weak agent. Used by **mneme-recall** and **mneme-adversarial**. The **`walk`
tool** is keyed by a `session` token (`walk{action:"start", db, start, budget}` →
`{session, view}`, then `walk{action, session, …}`); equivalently **`mnemed repl
<start> [--budget N]`** over a process. The walk is **read-only — it trains nothing**,
so a browse can't skew the graph (whoever drives it).

| action / repl command | effect |
|---|---|
| `look` / `edges` / `body` | inspect the **current** node only (travel to read a body; `view`/`edges` show top edges + a count) |
| `go` (`to: <i\|id>`) | step to a neighbor — **free** (no signalling); only the node budget gates it |
| `back` | retreat to the previous node |
| `done` / `abort` | end; both return the **trail** (`node`, previous `from`, exact stored `edge_from`/`edge_to`, `incoming`); MCP `done` also returns a single-use training **receipt**, while `abort` does not |

Budget caps **distinct nodes visited** (backtracking/revisits free). Edges carry an
`incoming` flag (reached against the edge's arrow). Over MCP, sessions are server-held
per-token, so several walks run at once.

**Training is post-factum and opt-in, via `reflect`** — once you know what actually
fed the answer, not a guess made mid-walk. Recall and browsing never grant themselves
write authority: call `reflect` only when the user explicitly requested memory
training or an already-authorized workflow includes it. MCP feedback uses opaque
server-issued receipts, so a caller cannot invent a trail and train arbitrary edges:

```
reflect{db, receipts:[<receipt from walk done>], used:[<ids that informed the answer>], unhelpful:[<explicitly unhelpful ids>]}
```
`used` earns positive credit; optional `unhelpful` banks interference. Omitted nodes
are unknown and receive neither. The two sets must be disjoint and contain only
nodes visited by these receipts. Feedback follows their exact stored routes;
duplicate arrows train once per batch, and conflicting judgments on one arrow skip
that edge's training while preserving each node's judgment. Incoming hops retain
their stored arrow; reflection never invents the opposite direction or recreates a route deleted after
the walk. A judged start (or another visited node with no first-visit route) receives
node-only feedback, preserving grounded-use telemetry without admission changes. Group receipts
by database: one reflect can combine at most 8 receipts and 64 total ids across
`used` and `unhelpful`, all from that same db. Receipt routes drive edge feedback;
**`used` means "contributed to the output," not "on-topic"**.
If you'd list nearly the whole trail as used, the query was too broad. Skip
`reflect` and no relevance training occurs. `query` and `recall` are currently pure:
they write no exposure, co-retrieval topology, or checkpoint. Post-pack exposure and
co-retrieval telemetry are not implemented. CLI REPL uses the selected owner's
walk/reflect; `done <used-id…>` requests positive-only grounded feedback. Bare
`done`, `abort` and EOF abort read-only without issuing a training receipt.

One MCP receipt batch commits atomically to one local store. Exact retry is
idempotent only inside the volatile issuing-host epoch while that host holds the
exclusive database lease. Saving leaves the live in-process proof intact, but
snapshot load/import and persistent-store reopen clear old feedback proofs. Restart
invalidates old receipts and cannot recover a lost pre-restart acknowledgement; do
not describe this as restart-retryable or distributed exactly-once.

After genuine positive walk feedback, `reflect` also invokes the consolidation hook
with `used`, but defaults disable speculative bridge minting
(`bridge_probability = 0`), so it normally creates no long-range edges. Receiptless reflection is refused,
including for operators. The no-op CLI `consolidate` route was retired; custom
engine/evaluation configurations can still exercise the experimental hook.

## Notes

- **Read capacity is not relevance.** The effort-sized source successor derives
  its offered window from explicit byte/work allowances, not an eight-card rule.
  Native request limits remain per-call fuses; a full packet is not a complete
  search. Observation metadata shares the native response budget, and only actual
  delivered cards can supply later evidence. More effort does not enlarge the
  foreground packet. Check installed schemas/configuration before relying on this
  opt-in integration behavior; see the [integration runbook](../../../integrations/codex/README.md#async-reader-opt-in-one-explicitly-selected-store).
- **Statuses.** `active` memories participate in ordinary recall immediately;
  `archived` memories remain inspectable but are excluded until explicitly restored.
  Stability is authored metadata, not eligibility or a truth score. Tagged retrieval
  uses bounded exact cosine for selective tags and an explicitly partial hybrid
  fallback above the work fuse; `query.v3` and `context.v7` report primary coverage,
  work, and projection watermark. Use `archived` for historical inspection.
- **The graph learns from grounded use.** `feedback` and `reflect` reinforce it;
  ordinary retrieval performs no mutation. `reflect` trains from server-issued walk
  receipts. The `walk` itself, `recall_context`, `query`, `recall`, `get`,
  `neighbors`, and `merges` are read-only — so browsing never skews the graph.
- **Full merge is local and operation-bounded.** It atomically normalizes the
  winner/loser incident sets (primary/outgoing plus `by_to` indexes in Cozo), archives
  the loser, terminally resolves the candidate, and records an exact-pair retry proof.
  Exact lookup is keyed; proof history grows linearly with committed pairs and has no
  lifetime-global count/cap. Import requires the exact candidate resolved `Full`, an
  archived retained loser, no local loser adjacency, and no loser-sourced remote edge.
  Other open overlays naming the loser are not yet redirected/blocked. The unsafe
  legacy partial-merge writer is removed from every public surface while persisted
  `Partial` verdicts remain readable. This is not a distributed transaction or the
  future typed redirect model.
- **Supersession is a one-time historical adjudication.** One local transaction
  installs the directional edge, archives the loser, resolves the contradiction,
  and records a durable canonical-pair proof. Same-direction replay is a no-op even
  after a later legal status/edge mutation; it does not reassert a standing invariant.
  Direct history reads remain available; ordinary recall and core loading exclude
  archived losers. Historical candidate losers are not migrated by retrying.
  `Unresolved` remains open and may later become either terminal verdict.
- **Bodies** live next to the db and resolve on explicit `get`/`body` reads or
  direct `core` loading; query and walk views return summaries.

### Touchstones

A normal SAVE note can own typed personal-meaning references. GET exposes the
summary-only historical record separately from current resolution; indexed
`list --touchstones` / MCP `list{db,kind:"touchstones"}` pages the collection.
Authorship belongs to the working session, not the librarian or a popularity score.
See [touchstones](../../../docs/touchstones.md) when authoring or inspecting one.
This uses a named storage successor and does not claim live rollout.

Installed boundaries: check the selected owner's actual catalog for native generation and optional hippocampus support. GET edge lists exclude episode incidents; use `episode references` when either endpoint is episodic.

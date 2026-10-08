# Continuity pilot v1

The source modules and synthetic fixtures are retained as experiment material.
This example is not an active Cargo target in this release (`autoexamples =
false`). Historical commands below require porting the example to current APIs and
explicitly registering its named `[[example]]` in a disposable manifest.
Retained rehearsal data does not qualify current runtime behavior.

This is a small executable evaluation of task effects across fresh actor phases.
It always uses independent `MemStore` instances, `InlineStore`, hashing embeddings,
a fixed clock, and deterministic IDs. No model, provider, persistent database or
live store is opened. These fixtures are coordinator material: do not give the
fixture directory, full artifact, authorship file or assessment to a fresh actor.

## Rehearse and validate

Run at the workspace root, with a disposable target directory if other feature
matrices are running:

```sh
export CARGO_TARGET_DIR=/private/tmp/mneme-continuity-pilot-target
cargo test --locked -p mneme-eval --no-default-features --example continuity_pilot
cargo run --locked -p mneme-eval --no-default-features --example continuity_pilot -- --rehearse --json > /private/tmp/continuity-1.json
cargo run --locked -p mneme-eval --no-default-features --example continuity_pilot -- --rehearse --json > /private/tmp/continuity-2.json
cargo test --locked -p mneme-eval --no-default-features
```

Compare `semantic_digest` and the complete parsed JSON objects. There is no
wall-clock timing in these artifacts. `rehearsal-summary.json` is the compact
checked-in result; regenerate full phase-level requests, responses, assessments,
configuration and costs with the command above. All recorded successes in that
summary are **scripted_rehearsal**, not fresh-agent evidence or a graph win.

## Fresh actor boundary

Build the example with the rehearsal command. Two equivalent interfaces exist:

```sh
$CARGO_TARGET_DIR/debug/examples/continuity_pilot --session --episode diagnostics --condition notes_search
$CARGO_TARGET_DIR/debug/examples/continuity_pilot --replay /private/tmp/session.jsonl --episode diagnostics --condition notes_search
```

`--session` accepts one JSON object per stdin line and flushes one response per
line. `--replay` rebuilds an independent in-memory session from the complete JSONL
request prefix and prints **only the final response**. A coordinator can append
requests to a private prefix, invoke replay, and pass just that last response to
the actor. Keep the prefix hidden from the actor; it includes coordinator controls
and may include assessment requests. Replay is a deterministic convenience, not
an inference or latency benchmark. Use a fixed copy of the executable for a run.

The episode is `diagnostics` or `async`; the condition is `notes_search`,
`hybrid_graph_off` or `hybrid_authored_graph`. Start with:

```json
{"op":"packet"}
```

The response supplies the **current phase only**, current handoff, available task
operations and memory protocol. Pass that response to a new actor with no previous
phase transcript. Have it choose iterative operations and finish. Supported
memory requests are:

```json
{"op":"search","query":"actor-selected text"}
{"op":"read","id":"ID returned by a prior result"}
{"op":"neighbors","id":"ID returned by a prior result"}
{"op":"finish","rationale":"why these actions were selected","used_memory_ids":[]}
```

The actor may use any of the task actions declared by its packet. Task operations
return actual model effects; `finish` returns no assessment. Memory IDs are engine
results, never gold-answer IDs. `re_explain` records an unanswered request for
human clarification. No human answer or model inference measurement is fabricated.

Coordinator-only requests are:

```json
{"control":"grade"}
{"control":"advance"}
{"control":"artifact"}
```

`grade` and `artifact` contain private assessment data. The process distinguishes
these by protocol keys, not authentication. **The driver must reject any actor
request containing `control`**, keep coordinator responses out of actor context,
and expose only the next `packet` after a transition. Spawn a new actor for every
phase while retaining the same condition's request prefix. The four phases are
learn, transfer, revise and return; no advance exists after return.

A coordinator may use `{"control":"advance","force":true}` if the actor cannot
finish, including after budget exhaustion. The incomplete phase remains recorded
as nonpassing; force does not manufacture task completion. A final incomplete
phase can be recorded with `artifact` without an advance.

## Conditions, authorship and budgets

Each condition provides the same phase-scoped, frozen summaries, bodies and
current handoff. `*-authorship.json` is one synthetic authorship trace authored
from scripted learn observations and the revise phase's public API notice. It is
frozen before the run and replayed only after those phases. It is **not** natural
note authorship by the evaluated fresh actors. `common_authorship_hash` must match
across conditions. No expected answer or assessment is an input to ingestion,
retrieval, task actions or actor packets.

Notes search offers iterative case-insensitive term search over all available
summaries and bodies, plus reads; an empty query lists the first four records.
Hybrid conditions use the current engine retrieval with reference hashing
embeddings. `hybrid_graph_off` disables traversal. `hybrid_authored_graph` adds only
the separately recorded phase-appropriate authored associative links; no synthetic
similarity links or learned graph condition is introduced. Supersession relations
and lifecycle effects exist in every condition.

Each phase has the same envelope: 32 admitted calls and 65,536 serialized JSON
request-plus-response bytes, including the starting packet. The maximum response
is 8,192 bytes, reserved before operations execute. Counts exclude line delimiters,
driver prompts and model inference. Requests rejected by the envelope execute no
operation but are logged with separate rejected-call and wire-byte counters; a
driver should stop the actor when `budget_rejected` appears. Artifact metrics
include actual admitted and rejected wire bytes. Provider tokens, provider cost
and natural authoring effort are explicitly unavailable.

Controlled authorship is replayed outside actor budgets and its exact requests,
results and bytes are reported separately. The common trace costs three ingests
and one supersede call per episode; the graph condition adds two link calls. This
controls content availability, not natural authoring quality or cost.

A separate `--independent-authorship` mode disables frozen replay and allows an
actor to submit `note` (summary/body), `link` (from/to) and `supersede`
(winner/loser) requests within its ordinary envelope. This condition must not be
reported as the controlled comparison. Correction uses the supported engine
workflow: ingest a successor, then separately supersede the previous note. The
old node remains readable, has reduced confidence and is a candidate; hybrid
recall may expose it in the probationary lane. The pilot preserves that behavior.
Ordinary search has no feedback receipt, so `feedback` returns an explicit error.

## Interpretation limits

Task results are explicit state and scheduling traces. Diagnostics grades actual
creation, observation-backed reports and required opener execution. Async ticks
model a shared loop: any pending direct synchronous fetch blocks the heartbeat.
There is no wall-clock scheduler or real application performance measurement.
Skipping the new opener and dispatching native async work to a worker are recorded
as obsolete-workaround **proxies**, not proof that old advice caused the action.
Keep note exposure, the actor's self-reported memory use and resulting effects
separate. A successful script validates the boundary and fixtures; fresh actors
and later held-out tasks are needed for product claims. The graph is not required
to outperform searchable notes.

## Investigation v2

The additive `--investigation` flag selects the versioned source/contract variants
and `empty_memory` / `notes_search` conditions. Notes are naturally authored through
the charged independent operations in learn/revise; no v1 fixture notes are replayed.
The original commands and v1 digest remain unchanged. See the coordinator-only
[investigation run protocol](investigation-v2/RUN-PROTOCOL.md) for the frozen
16-actor design, discovery operations, source identities and progression rule.

```sh
cargo run --locked -p mneme-eval --no-default-features --example continuity_pilot -- --rehearse --investigation --json
```

This produces a deterministic four-episode plumbing rehearsal; it does not run
actors or establish that memory saves work. Both conditions can directly inspect
current contracts or use a one-call disposable probe, and a cheap control is valid.

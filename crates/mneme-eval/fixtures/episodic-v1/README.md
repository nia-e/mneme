# Episodic memory v1: synthetic acceptance scenes

The source modules and synthetic fixtures are retained as experiment material.
This example is not an active Cargo target in this release (`autoexamples =
false`). Historical commands below require porting the example to current APIs and
explicitly registering its named `[[example]]` in a disposable manifest.
Retained rehearsal data does not qualify current runtime behavior.

Eight invented experiences, three semantic lessons, and one editorial correction.
Rin, Ari, Lantern, Rainmap and the beacon are fictional. **Never import these
records into a live or personal store.** This tests a small memory design, not
natural authoring quality, a graph advantage, or model intelligence.

`corpus.json` contains authored material and its insertion schedule.
`assessment.json` contains coordinator-only mechanical expectations and qualitative
rubrics. Neither file belongs in a recall actor's context. This is an evaluation
boundary, not filesystem secrecy: the driver must expose only bounded read tools.

## Fixture contract

This JSON is a **fixture format, not the public episode API**. The loader must use
the actual typed operations; do not add fixture fields to production parsers.

- `episodes` are initial editions. `semantic_notes` are ordinary semantic captures.
  `editorial_revisions` create new immutable editions under an existing root.
- `schedule` supplies a monotonically increasing test-clock value in `at_ms` for
  each authored operation. All times are Unix epoch milliseconds in UTC, not IDs.
  Episode occurrence intervals are separate from this recording/creation clock.
- Resolve each `alias` to IDs returned by earlier successful operations. `links.to`
  names an exact edition or semantic node. A revision's `root` names the stable
  episode root; `expected_edition` names the previous head. Source keys remain
  distinct across editions, while identical replay retains the original key.
- Semantic supersession is a separate `supersede_semantic` operation. It must not
  be smuggled into a capture link or implemented by editing the earlier episode.
- `reason` describes the authored editorial change. Map it to the finalized
  revision contract if supported; otherwise retain it in the fixture operation
  trace. Do not invent a new production field to accommodate this fixture.
- `as_of_ms` freezes the current time used in actor packets and rehearsal output.

Use an independent reference `MemStore`, `InlineStore`, hashing embeddings and
an explicitly advanced test clock. Disable similarity-generated links: only the
authored evidence relationships belong in this exercise. No provider call, model
download, network service or persistent user database is needed.

The scenes include a mistaken explanation, local success followed by a changed
recommendation, an unfinished import, a shared moment with no lesson, a late
recording, tied times on concurrent threads, and an unreviewed prototype. The
misspelling `amreb` is intentional: an explicit editorial revision corrects it
to `amber`. The original edition remains readable; root reads resolve the new
edition. Editing is not another experience and must not move the original event
to the top of an occurrence-ordered timeline.

The v1 cue lane is **lexical matching over current-edition summaries**, not hybrid
semantic episodic search. Ordinary semantic recall can instead retrieve a lesson
and deliberately follow its episode references. Do not grade lexical results as
semantic-search efficacy or manufacture an exact relevance order for paraphrases.

## Two kinds of evidence

The mechanical rehearsal checks the assertions in `assessment.json`: round-trip,
time and thread views, paging, typed lane separation, immutable editorial history,
retry/conflict behavior, semantic supersession and maintenance boundaries. Run it
twice and compare semantic artifacts. Scripted passing results are labelled
`scripted_rehearsal`, never fresh-actor evidence. The separate `continuity_pilot`
fixture and its frozen digest must not change for this addition.

Run the isolated executable from the workspace root:

```sh
cargo test --offline -p mneme-eval --no-default-features --example episodic_memory_v1
cargo run --offline -p mneme-eval --no-default-features --example episodic_memory_v1 -- --rehearse
```

The rehearsal exercises the shared `mneme-app::episode::PreparedEpisode` surface.
Its export/import check preserves the canonical store and reuses the in-memory
body resolver; it is not a self-contained backup or persistent-store migration.
Content digests replace randomly allocated InlineStore body URIs in the semantic
repeatability artifact. The underlying exact export still round-trips unchanged.

The actor interface reconstructs the frozen synthetic store for each read. Only
the isolated session's counters, request/response trace and final packet persist.
Copy the built executable to a fixed run directory before fresh actors start:

```sh
# Coordinator only; parent directory must already exist, session directory must not.
RUNNER --init /private/tmp/episode-actor-a recent_activity
# Expose only this command shape to the actor, with its assigned directory fixed.
RUNNER --request /private/tmp/episode-actor-a '{"request":{"action":"list","limit":2}}'
RUNNER --finish /private/tmp/episode-actor-a '{"answer":"...","episode_ids":[],"lesson_ids":[]}'
# Coordinator only, after the actor finishes or exhausts its envelope.
RUNNER --artifact /private/tmp/episode-actor-a
```

Other case IDs are `changed_recommendation` and `similar_situation`. `RUNNER` is
the frozen executable, not a shell command supplied by the actor. The driver
must not expose `--init`, `--artifact`, arbitrary paths or filesystem reads as
actor tools. Initialization never resets an existing session. State is locked
per request, pinned to the fixture digest, and saved before responses are emitted.
If a process dies holding `request.lock` or `session.pending`, leave that attempt
incomplete for the coordinator to inspect rather than silently resetting it.

## Fresh recall actors

Use three new read-only subagents, one per question in `assessment.json`. Each
gets only the question, `as_of_ms`, a short note that these are Rin's synthetic
memories, the available read-operation descriptions, and responses to its own
requests. No inherited task discussion, other actor packet, fixture directory,
source aliases, corpus dump, loader trace or assessment is provided.

Actor tools may perform bounded timeline and lexical cue reads, exact/root reads,
semantic recall and explicit reference exploration through the finalized interfaces.
They cannot mutate memory or run coordinator grading. Delegation belongs to the
test driver, not Mneme storage. No actor result is written back into memory.

Enforce **12 memory calls, 8,192 cumulative tool-response bytes, and a 250-word
final answer**. Charge errors as reads. Reserve response space before issuing a
read; if its envelope cannot fit, stop explicitly rather than deliver partial JSON
or silently drop bytes. Record actual calls and serialized response bytes. Driver
prompts and inference tokens are separate, not fictional zero-cost measurements.
Malformed admitted JSON requests and oversize response errors consume one call;
once exhausted, further requests execute no read. `--finish` remains available.

Ask for a compact recollection packet: an answer, a few actual returned episode
or lesson references, and any relevant uncertainty or correction. A lesson is
optional. Accept sensible paraphrases; score the four qualitative dimensions by
reading the actual answer, not by searching for exact gold wording. Report hard
failures separately from numeric scores and preserve incomplete runs as incomplete.

The three questions exercise recent activity, changed advice, and a similar new
situation. Passing this tiny authored exercise supports a bounded acceptance claim;
it does not establish production retrieval quality or that every experience should
be captured automatically. Request-driven artifacts remain labelled
`request_driven_unclassified` until a coordinator attaches actual fresh-subagent
provenance; running the driver alone is not evidence that a fresh actor participated.

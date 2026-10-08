# Continuity investigation v2: frozen run protocol

This is an authored synthetic investigation protocol.
This protocol is coordinator-only; give actors only their current packet and the
operation client described below. Do not expose this directory or rehearsal notes.

## Question and scope

Can an agent's naturally authored checkpoint repay writing and lookup costs when
a successor faces the same dependency, a changed version, and a mixed return?
The two sequences are diagnostics and async boundaries. The four phases are
learn, transfer, revise and return. There are two conditions, for **16 fresh actors**:

- `empty_memory`: current task and source/probe access, empty episode stores;
  `note` and `supersede` are denied and charged if attempted.
- `notes_search`: identical task and source/probe access; starts empty and retains
  only notes/corrections naturally authored by this episode's learn/revise actors.
  Search covers summaries and bodies; a search returns at most four cards.

No supplied gold notes, actor-visible future tasks, mandatory note/discovery calls,
graph condition, synthetic distractor quota or outside authoring intervention is
admitted. A successor can solve the task without memory. Natural note quality is
part of the experiment: do not repair a poor note after inspecting outcomes.

## Artifacts and task contract

`diagnostics.json` and `async.json` hold private state and public current objectives.
Each `*-sources.json` contains immutable versioned adapter source and dependency
contracts. These are small simulated dependencies, not claims about production
Mneme, database libraries or runtime scheduling performance. Task effects execute
the existing deterministic environment and private `../assessment.json` grades
those effects; the assessment is never an input to actor discovery or task actions.

The public packet identifies current components, dependency family, version and
entry source path. It does not identify the hidden API behavior. Every current
entry source points directly to its dependency contract. `source_search` can list
all current artifacts, including contracts; reading a contract directly is allowed.
A single `probe` observes a fresh disposable opener or direct/worker fetch with
before/after or scheduling results. Probes cannot alter actual task state or
substitute for the requested actual task observations. There is no required
source-reading order, source count, probe count, password or scavenger hunt.

Both arms receive the same available source and probe results. Learn/transfer
expose release 1.4.0, revise exposes 2.0.0, return exposes both current installed
versions. Prior source files are unavailable when their version is no longer
installed; both conditions can inspect every fact needed for their current task.
Retained version-scoped notes may remain useful for the mixed return.

Diagnostics grades preservation and observation-backed missing/usable reports;
usable requires a successful open. The current objective requires exercising an
opener on absent storage when its contract permits non-creating diagnostics.
Async grades completed work, observed heartbeat progress and justified worker use.
Its heartbeat model is shared: another pending direct blocking fetch blocks the
loop. Graded repeated mistakes and obsolete-workaround proxies remain separate
from claims about memory causing behavior. Discovery is optional for correctness.

## Equal execution envelope

Use `gpt-6-astra` with `ultra` reasoning for every actor, matching the earlier pilot.
Record the actual host model/effort instead of assuming configuration was honored.
If unavailable, stop before actor execution and make a new common declaration;
never silently substitute only some actors. Start each with `fork_turns="none"`
and no previous phase transcript. Four episode sessions persist independently.
Do not reuse note content between episodes or arms.

Each phase admits at most 32 calls, including packet and finish, and 65,536
serialized JSON request/response bytes. The 8,192-byte maximum response is reserved
before effects. All attempted actor operations are retained. Errors within the
budget count as admitted calls. Budget rejections execute no effect but count as
attempted protocol calls and wire bytes; stop the actor at its first budget rejection.
The coordinator may force an incomplete transition. Do not retry with a fresh
budget or silently replace an unfinished actor. Coordinator controls are excluded.
These limits do not bound provider inference tokens or hidden host context.

A note (at most 512 summary bytes and 2,048 body bytes) and a separate `supersede`
are ordinary charged operations. Learn/revise may author naturally; transfer/return
cannot alter history. Supersession on this frozen source halves the old note's
confidence (a freshly authored 0.5 note becomes 0.25) and makes it a candidate;
it stays readable and searchable.
Do not import a different branch's lifecycle semantics mid-run. There are no graph
operations. `finish.used_memory_ids` and rationale are self-reports, not grades.

## Freeze before dispatch

Build and test once, then copy the executable to a private disposable run directory.
Record exact HEAD, working-tree patch and SHA-256 of that patch, the executable,
`continuity_pilot.rs`, every example module, every v2 fixture, this protocol,
`../assessment.json`, `Cargo.lock`, and the relevant crate manifests. Retain the
runner's fixture and protocol identities from a coordinator artifact. Source/body
hashes in protocol results identify JSON-encoded content; whole-file freeze hashes
must additionally identify the actual bytes on disk. Record the source tree as
base plus patch, not merely the base commit. No source or binary edits after freeze.
An evaluator correction discovered before dispatch requires a new freeze and
rehearsal. A correction discovered after dispatch requires a new declared run;
retain the original evidence and do not combine the results.

The coordinator keeps one private append-only JSONL request prefix per episode and
condition. Save a copy and its SHA-256 after every actor, plus the private grade,
artifact, exact brief and actor identity. Every phase artifact records authored
history hashes at start/end; the next start must equal the preceding end. The
history consists of successful natural note/supersede operations and their actual
responses. Failed authorship attempts remain in the phase log and budget. No
coordinator insertion or editing of authored history is allowed.

## Driver and actor dispatch

Build and deterministic rehearsal:

```sh
cargo test --locked -p mneme-eval --no-default-features --example continuity_pilot
cargo test --locked -p mneme-eval --no-default-features
cargo run --locked -p mneme-eval --no-default-features --example continuity_pilot -- --rehearse --json > /private/tmp/continuity-v1.json
cargo run --locked -p mneme-eval --no-default-features --example continuity_pilot -- --rehearse --investigation --json > /private/tmp/investigation-v2-1.json
cargo run --locked -p mneme-eval --no-default-features --example continuity_pilot -- --rehearse --investigation --json > /private/tmp/investigation-v2-2.json
```

Compare both complete parsed v2 objects, not just summaries. V1 must retain digest
`1dc8f4b1346e0079ce5445fccbe7a3878f6300dae9f03828a71042f3c73fd5f0`.
Scripted v2 rehearsal writes its own notes through public operations; these are
plumbing evidence and must never be preloaded into the real actor episodes.

Invocation with a frozen binary (example path):

```sh
/private/tmp/continuity-investigation-frozen --session --investigation --episode diagnostics --condition empty_memory
/private/tmp/continuity-investigation-frozen --replay /private/tmp/private-diagnostics-notes.jsonl --investigation --episode diagnostics --condition notes_search
```

Both interfaces use the v1 JSONL protocol. The first actor operation is
`{"op":"packet"}`. The coordinator alone can send `grade`, `advance` and `artifact`
controls. This separation is **not authentication**: the actor's operation client
must reject any `control` key. Give the actor no shell/read access to the full
request prefix, executable sources, fixture directory, grade, history or other
actors. It may read only its assigned brief and call its operation client, which
appends one permitted request and returns only that request's response. Audit
actual actor tool commands and reject a contaminated run; do not rely on prose
instructions as an isolation mechanism. Audit/refused client attempts are retained
separately; they cannot become free successful work. Never expose the private
artifact via error messages.

Use the same neutral brief in both arms, apart from client path and current packet:

> Complete the task using the current packet and the episode operation client.
> Choose source reads, disposable probes, memory lookups and task actions as useful.
> Ground reports in actual task observations. Available checkpoint operations are
> optional and charged. Finish when done. Only this phase is visible; do not inspect
> repository files, other tasks, hidden history or coordinator controls. If an
> operation exhausts the phase budget, stop and retain the unfinished outcome.

Run one actor per phase. After its final response, save grade and artifact, freeze
the prefix/history, advance once and give a fresh actor only the next packet.
Counterbalance condition order without changing tasks: diagnostics runs empty
then notes within each phase, async runs notes then empty within each phase. Phases
remain chronological within each independent episode. Do not reveal other actors'
choices or comparative metrics before all 16 actors finish. No replacement actors.

## Measurements and decision rule

Retain phase success, incomplete/budget outcomes, errors and rejected attempts,
per-component graded repeated mistakes and obsolete-workaround proxies, raw calls
and request/response bytes. The exclusive categories are:

- discovery: `source_search`, `source_read`, `probe`;
- memory access: `search`, `read` (denied graph-access attempts remain visible);
- authorship: `note`, `supersede` (denied graph-authoring attempts remain visible);
- task: actual environment actions, including task inspection/heartbeat observations;
- protocol: packet, finish, re-explanation and unknown operations.

Report admitted and attempted counts and failures for each category. A task
observation is execution, not a second discovery count. Discovery totals use all
attempted discovery calls, including errors and budget rejections. Whole-episode
protocol totals use all attempted actor calls, including authoring, memory access,
packet, finish, errors and budget rejections. Preserve admitted totals alongside
these so failures cannot disappear behind one number.

Separate learn/revise authoring costs from transfer/revise/return reuse and checking
costs, then show whole-episode totals including both. Capture actual per-actor host
inference input, cached input, output and reasoning usage when available; leave it
unavailable when not observed. Report protocol bytes and inference separately.
Shared profile, host instructions and pretrained knowledge are present in both
arms; "empty memory" refers only to the episode stores. Replay elapsed time is not
retrieval latency. No provider-cost or application-performance claims follow.

Proceed to a bounded Mneme-versus-notes comparison only if **both sequences** meet
all of the following:

1. Notes passes all four phases with no graded repeated mistake or obsolete
   workaround proxy.
2. The control passes all four phases. A failed control is a quality outcome for
   separate analysis, not a valid denominator for claiming saved investigation.
3. Let C and N be control and notes discovery calls summed across transfer,
   revise and return. C must be nonzero and `(C - N) / C >= 0.25`.
4. Notes uses no more total protocol calls across the entire episode than control,
   including authoring, corrections and all memory access.

A zero-discovery successful control supplies no discoverable-work saving. The
one-call probe is intentionally cheap: a control using it is a valid result, not a
reason to add friction. Call savings do not establish byte, compute, money or
latency savings. This is an exploratory progression rule for two sequences, not a
general efficacy threshold. If any requirement fails, retain the result and locate
the cost/failure in writing, finding, checking or applying experience. Do not tune
these tasks until memory wins; a revised hypothesis needs a new declared run.

# EBR-Bench: preparation protocol

**Status: protocol only; executable integration blocked on upstream access.**
No benchmark, model, embedding, download, or live-store operation has been run
for this setup. No verified public link to Epoch's engine and required assets
was found in the official benchmark, launch, and update pages reviewed. This is
a bounded search, not a claim that access is impossible. Epoch says it does not
distribute the game's copyrighted material. Obtain a legitimate engine/assets
access route before implementing an adapter; do not invent one or request secrets.

## Freeze the upstream contract

[Epoch's methodology](https://epoch.ai/benchmarks/ebr-bench) specifies ten games
per independent sample: eight learning games, then two scored games. The topline
is the better final score, expressed out of 21 objectives. Match v4's card ban,
default single-agent setting, and 250k-token compaction threshold; record any
documented model-specific exception. Freeze engine/assets revisions, prompts,
tool contract, scoring rules, and actual model/effort identifiers before a run.
The [v4 update](https://epoch.ai/publications/ebr-bench-update) changes game content
to address a ceiling effect: pre-ban results are not a clean matched comparison.

## Proposed matched comparison

Use the same actor, effort, game rules, within-game scratchpad, and limits:

- **Memory off:** discard cross-game notes and memory after every game.
- **Persistent notes:** upstream's ordinary cross-game notes mechanism.
- **Mneme:** replace cross-game notes with explicit Mneme save/recall; do not
  quietly give this arm both persistent notes and Mneme.

Give every arm an independent sample environment. Pair seeds only if the upstream
engine supports reproducible seeds; otherwise record that pairing is unavailable.
Keep each Mneme sample's fresh isolated database/body store across its ten games,
then never reuse it for another sample or arm. Keep within-game scratch identical.
Reset actor context between games according to the verified upstream contract;
only the arm's assigned memory may carry experience forward. Host transcripts,
scores, and receipts must not become an extra readable memory channel.

These are local ablation arms, not a claim of official leaderboard equivalence.
Do not relabel a cheaper/weaker model alias as the intended model. A pilot's
smaller sample count or budget must be conspicuous and cannot establish parity.

## Minimal adapter, once access exists

Place a small upstream adapter and preparation checks in `tools/ebr_bench/`, with
tiny provider-free fixtures in `tools/testdata/ebr-bench/`. Plug into the verified
engine rather than cloning an old Codex/Mindcraft harness. Preparation should
validate and freeze inputs without launching games; execution needs an explicit
separate action. Missing upstream inputs must stop preparation, not trigger a
download, model call, or guessed fallback.

Use disposable HOME/XDG/config/cache directories and an explicit isolated owner.
Scrub inherited Mneme configuration and constrain project discovery; never open
the global database, enrolled project stores, or production services. Reuse the
isolation pattern in `tools/cli_owner_smoke.py`, not its full smoke workflow.
Remember that `mneme-eval` defaults to real embeddings: it is not a no-model check.

## Receipts and review before execution

Record per-game objectives and the learning curve, plus final best-of-two /21.
Report independent sample count, uncertainty, failures, and omitted coverage.
Record cached input, uncached input, output tokens, cost, and wall time, including
any librarian and embedding work; unknown accounting stays explicitly unknown.
Label actor limits, memory/context limits, compaction, and total budget separately.
Preserve input/artifact hashes and arm definitions. Review the adapter's reset,
memory-isolation, accounting, and no-execution preparation checks before running.

A public implementation of a different game, such as ArkhamBench, is a separate
experiment—not a substitute engine or a source of official EBR-Bench scores.

An EBR-inspired subset is another possible experiment if official access remains
unavailable. Preserve the repeated-play learning structure, but label its rules,
engine and scores as our own; the possibility is parked in project memory, not
an active implementation task.

# Working on Mneme

Mneme helps successive agents continue work, reuse experience, and revise what
no longer applies. Ordinary **save, recall and inspect** should be straightforward.
A graph is a means, not the product. See the [documentation map](docs/README.md)
and [architecture](docs/architecture.md).

## Design criteria

- Keep meaning separate from policy: `mneme-core` owns domain types, validation
  and ports; `mneme-engine` owns retrieval/learning policy; `mneme-app` owns shared
  workflows; CLI, MCP and integration code adapt those workflows. Persistence,
  inference and body access belong behind their adapters. Do not rebuild the same
  operation independently in each frontend.
- Encode real distinctions and invariants in types and checked constructors.
  Prefer coherent owners over giant files or a forest of pass-through modules.
  Extract a responsibility, not an arbitrary line range; do not introduce a
  framework for hypothetical callers. Use traits for real contracts or
  replaceable implementations, not as a prerequisite for ordinary functions.
- Distinguish source/provenance, retrieval relevance, observed usefulness and
  authored personal significance. None is a truth score. Episodes are accounts
  of what happened, not automatically advice for today; exact editions must not
  silently become their successors. Touchstones preserve deliberately authored
  meaning; popularity must not turn into identity or core membership.
- Reads do not reinforce, create learning evidence, or mutate topology. Not
  seeing or using a memory is not, by itself, negative feedback. Missing delivery
  is not an empty store; an unavailable owner is not deletion.
- Scope and ownership are explicit. A project operation must not fall back to the
  private global store. Use the existing owner rather than opening its live store
  with another process. Libraries compose reads without becoming extra writers.
- Bound aggregate work, bytes, context and inference—including retries, pages and
  queues. Report partial coverage and omissions honestly. Derive limits from the
  resource envelope, not arbitrary quotas on how many relevant memories may exist.
- Treat embeddings, projections and ranking policy as replaceable machinery.
  Preserve authored content, identities, revisions, sources and non-rebuildable
  learned state unless a deliberate migration says otherwise.
- Design for owned, trusted-but-messy local memory: stale, contradictory and
  underspecified notes are normal. Preserve ordinary validation, safe paths,
  atomicity and recovery without making a hostile multi-tenant service the
  default threat model or a prerequisite for ordinary use.

## Changes and compatibility

- Inspect the actual checkout and relevant instructions before editing. Preserve
  concurrent work. Source, installed runtime and live store are different states.
- Pre-1.0 compatibility serves **current integrations and documented operator
  workflows**, not every historical prototype. Before removing old-format
  readers or migrations, inventory configured and retained stores, including
  other configured devices; migrate needed stores with paired database/body
  backups, identity checks and a verified recovery path first. Never infer an
  empty inventory from a single local checkout or rewrite immutable evidence.
- Checkpoint source before broad cleanup. A Git commit preserves code and a
  migration path, not live data: rollback also needs the matching runtime,
  configuration and database/body pair. Do not downgrade post-migration writes
  by swapping only the binary.
- Use the [change-gates skill](.agents/skills/mneme-change-gates/SKILL.md) for its
  declared CLI/MCP, authority, transport, storage and compatibility boundaries.
  Select the smallest applicable checks; do not turn unrelated edits into a
  release ceremony. Do not change services, hooks or enrolled stores implicitly.

## Evidence and documentation

- Run focused tests for changed behavior and plausible regressions, then broaden
  where the risk or failures justify it. Use disposable stores for experiments.
  Do not require a benchmark marathon for a refactor or a clear interface change.
  Share fixtures through test support, not another runnable test suite. Keep
  distinct backend/transport/failure-boundary coverage; consolidate duplicate
  inventories and test pure validation without subprocesses when possible.
- Match claims to evidence: source inspection, component tests, copied/installed
  artifacts, live-store checks and workload quality establish different things.
  A negative result is useful; do not retune a frozen test into a win.
- Put user/operator instructions in guides and keep active handoffs short.
  Preserve the paths and hashes of evidence supporting a claim. Local `target/`
  receipts are not portable checked-in artifacts. Label unfinished proposals
  honestly rather than claiming their exit criteria were met.

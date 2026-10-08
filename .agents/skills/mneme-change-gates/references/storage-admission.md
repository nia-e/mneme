# Storage admission, open, and lease gate pack

Apply this pack before a persistent database is admitted, created, opened, leased,
released, resumed, or selected as a cross-database target.

## Required gates

- **OPEN-01 — One resolved identity:** Resolve configured/default paths, activation
  selectors, and existing aliases before lease or open. Never fall back to a new
  conventional database when activated, torn, or unknown state exists.
- **OPEN-02 — Path provenance:** Reject direct private-generation configuration,
  unsafe symlink/hard-link identities, multiply linked stores, and namespace swaps.
  Aliases of one admitted database contend on one canonical adjacent lease.
- **OPEN-03 — Lease capability:** Hold the exact lease across every persistent
  handle and detached job. Mutation proves its guard; release/resume quiesces work
  and rotates volatile epochs. An `Arc` preserves lifetime, not sibling serialization.
- **OPEN-04 — Positive classification:** Distinguish absent, supported current,
  explicitly supported legacy, torn, managed, predecessor, and unknown stores.
  Existing-only admission never creates. Unknown state fails closed without a
  permissive legacy fallback.
- **OPEN-05 — Side-effect order:** Perform classification and authority checks before
  schema creation, feedback-epoch activation, sidecars, parent creation, or target
  mutation. Failed admission leaves no misleading empty store.
- **OPEN-06 — Frontend completeness:** CLI, MCP, resume, recovery verification, and
  cross-database routes use the same resolver/classifier/lease policy. Read-only
  status operations do not mutate retry or feedback ledgers.

## Sibling attack

Probe absent/existing current/legacy/torn paths, relative and symlink aliases,
same-database cross-links, typo targets, concurrent open, release/resume, detached
work, and every frontend. If the change renames or activates a generation, add
`storage-publication`; if it changes accepted old artifacts, add
`stale-writer-compatibility`.

## Evidence

Minimum: focused persistent component tests plus the affected frontend suites.
Include contention, no-residue refusal, canonical alias, reopen, and release/resume
tests. Installed or live-store claims require their higher evidence tiers.

Apply this pack directly; it is the admission-contract workflow for this change.

# Storage publication and recovery gate pack

Apply this pack to detached construction, rename/swap, selector activation,
durability barriers, crash-state classification, recovery, and residue cleanup.

## Required gates

- **PUBLISH-01 — Detached bounded preparation:** Build in a private operation-bound
  stage with a closed inventory and bounded validation. Do not mutate the selected
  generation in place.
- **PUBLISH-02 — Exact identity/no-clobber:** Bind operation, source, target, database,
  schema, and expected absence before crossing. Publication never overwrites an
  unrelated target or deletes unrecognized evidence.
- **PUBLISH-03 — Continuous authority:** Retain typed stage/publication/recovery
  capabilities and the exact lease through every fallible boundary. A parent rename
  may preserve flock continuity while invalidating old pathname guards; do not use
  `?` where it would drop the only recovery authority.
- **PUBLISH-04 — Durability order:** Sync payloads and records before directory
  publication, then sync the renamed target and parent before claiming durable
  publication. Selector activation has its own durable barrier.
- **PUBLISH-05 — Coherent visibility:** Verify the complete target before activation.
  Readers observe a complete old or complete new generation, never a mixed schema or
  selector naming unverified bytes.
- **PUBLISH-06 — Typed recovery:** Distinguish not-published, crossed/ambiguous,
  durable-but-unguarded, and durable-with-residue states. Retry only exact recognized
  states; preserve ambiguous evidence for managed recovery.
- **PUBLISH-07 — Boundary attacks:** Exercise failpoints before/after every write,
  sync, rename, guard, verification, selector switch, and cleanup. Prove exact retry,
  no-clobber, cleanup inventory, and reopen through normal frontends.

## Sibling attack

Check bootstrap, refresh, repair, migration, re-embed/index publication, rollback,
selector recovery, and both persistent backends. Probe same-UID competitors,
lost-ack retries, residue, cancellation, and a process dying at each durable cut.
Add `storage-admission` when normal opens consume the publication and
`stale-writer-compatibility` when old artifacts can observe or mutate it.

## Evidence

Minimum: component tests with deterministic failpoints and persistent reopen. Claims
about installed recovery commands or a real database require `installed_artifact`
or `live_store` evidence respectively.

Apply this pack directly. Repository graph initialization still routes to
`mneme-bootstrap`.

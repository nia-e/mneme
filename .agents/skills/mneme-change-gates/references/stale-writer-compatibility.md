# Stale-writer and compatibility gate pack

Apply this pack when a format, catalog, projection, generation, epoch, migration, or
retry rule changes what old and current artifacts may read or write.

## Required gates

- **COMPAT-01 — Explicit matrix:** Enumerate old/current reader and writer behavior
  against absent, legacy, current, predecessor, managed, torn, and unknown stores.
  State the supported horizon; “backward compatible” without a matrix is bullshit.
- **COMPAT-02 — Commit-time binding:** Bind mutations to the admitted database,
  format/catalog generation, projection generation, and volatile/durable epoch they
  require. Recheck the relevant fence immediately before commit or publication.
- **COMPAT-03 — Durable old-writer fence:** Once new semantics publish, an old writer
  cannot clear, ignore, or race the fence into a successful stale commit. A lock held
  only by the upgrader is not a future-writer fence.
- **COMPAT-04 — Honest retry:** Distinguish exact replay from ambiguous lost
  acknowledgement and new intent. Restart/import/release invalidates volatile proofs;
  do not claim exactly-once beyond the persisted evidence.
- **COMPAT-05 — Deliberate migration:** Derive new state from canonical data, verify
  it before activation, and reject unknown partial states. Preserve compatibility
  only for named supported artifacts, not discarded local design debris.
- **COMPAT-06 — Real skew evidence:** Test genuine prior fixtures or binaries when
  claiming compatibility. Include old-reader refusal, stale-writer attempts, current
  reopen, failed migration recovery, and packaged feature/version skew.

## Sibling attack

Probe direct and receipt feedback, background/detached mutation, migration and
re-embed, CLI/MCP reopen, release/resume, snapshot import, and every backend that
shares the marker. Check both lost-ack and crash-before-ack retries. Add
`storage-publication` for new generation state and `transport-package` when old or
new installed binaries form part of the claim.

## Evidence

Minimum: component tests using a real prior fixture for every supported compatibility
claim. Add `installed_artifact` when behavior depends on old/new binaries and
`live_store` only for an explicitly authorized real migration.

Apply this pack directly; it is the compatibility-contract workflow for this change.

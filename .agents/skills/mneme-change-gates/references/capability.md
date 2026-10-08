# Capability gate pack

Apply this pack when visibility or authority differs by operation, profile, receipt,
or lifecycle state. A capability is not authentication merely because it is typed.

## Required gates

- **CAP-01 — Exhaustive matrix:** Enumerate every public operation against every
  capability profile. Make visibility, allowed dispatch, and denial explicit; no
  wildcard arm may silently inherit future operations.
- **CAP-02 — Early denial:** Reject absent authority after complete bounded parsing
  but before database checkout, embedder initialization, cold admission, detached
  work, or mutation. Do not mint a blanket capability for convenience.
- **CAP-03 — Discovery/dispatch parity:** Catalog schemas, CLI help, stdio dispatch,
  and HTTP dispatch agree for both allowed and denied operations. Denial errors are
  stable and do not disclose hidden payloads.
- **CAP-04 — Honest meaning:** Keep process capability, receipt proof, database
  lease, and caller authentication distinct. A receipt proves only its documented
  observation/epoch facts and cannot become ambient mutation authority.
- **CAP-05 — Lifecycle fencing:** Release/resume, restart, import, and database
  switching invalidate or rotate volatile authority exactly once. Detached jobs
  retain only the capabilities they need and cannot outlive the relevant fence.

## Sibling attack

Probe unkeyed feedback, topology/link mutation, curation, deletion, cold whole-graph
operations, aliases, and both catalog and raw dispatch. Check newly added operations
against every profile rather than testing only the default profile.

## Evidence

Minimum: an exhaustive table-driven visibility/dispatch test with proofs that denied
calls do not reach store, inference, or cold-work admission. Add transport/package
evidence when claiming parity of shipped stdio and HTTP servers.

Apply this pack directly; it is the public-authority workflow for this change.

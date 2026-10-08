# Public interface gate pack

Apply this pack to CLI and MCP request, response, schema, error, and recovery UX.

## Required gates

- **API-01 — Typed admission:** Parse the complete request into one bounded typed
  contract before database checkout, inference, cold-work admission, or mutation.
  Reject wrong types, overflow, invalid combinations, and lossy coercion.
- **API-02 — Catalog/dispatch agreement:** Help and MCP catalogs expose exactly
  what dispatch accepts. Removed or profile-hidden operations cannot remain callable
  through aliases or raw method names.
- **API-03 — Representation parity:** CLI human/JSON and MCP result/error shapes
  preserve the same admission and domain semantics. Serialization differences must
  be explicit and bounded.
- **API-04 — Break and recovery contract:** State intentional breaks plainly. Keep
  compatibility only for a named supported artifact or migration; do not preserve
  discarded design aliases. Refusals identify the database/operation and a real
  recovery action without leaking secrets.
- **API-05 — Agent affordance:** The normal workflow has a low-branch happy path,
  deterministic schemas, bounded output, and actionable refusal. Shipped docs and
  focused examples match the actual surface.

## Sibling attack

Check adjacent read/mutate commands, CLI aliases, MCP list/catalog and raw dispatch,
human/JSON modes, defaulted arguments, and the same request through release/resume.
If stdio/HTTP or packaging changes, add `transport-package`; if authority changes,
add `capability`.

## Evidence

Minimum: focused parser/dispatch tests plus the affected CLI/MCP aggregate suite.
Use bounded protocol fixtures. A packaged-surface claim also needs the
`installed_artifact` tier selected by `transport-package`.

Apply this pack directly; it is the public-contract workflow for this change.

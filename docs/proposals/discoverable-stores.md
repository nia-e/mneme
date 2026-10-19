# Discoverable stores through a shared registry

Status: future prototype; not implemented. Keep this after the current UX pass.

## Intent

Configure one bootstrap owner, not paths for every store on every client. When a
new store is created, it announces its existence to the MCP server for the global
store. Other clients ask that owner for the catalog and learn the stores' names,
identities and routes. A Nix configuration can then identify one bootstrap owner
rather than enumerate the whole collection.

The intended creation flow announces by default when a bootstrap owner is
configured, with a CLI opt-out. Exact flag
names and the bootstrap configuration shape are not settled. This is **not** a
filesystem scan or an invitation to enumerate arbitrary home directories.

## Small prototype

1. Create a disposable store and publish its descriptor to the bootstrap owner.
2. Start another client configured with only that bootstrap owner.
3. Show the announced store in its picker and open it through its actual owner.
4. Repeat with announcement disabled and with the bootstrap owner unavailable.

Reuse existing catalog/enrollment machinery where it fits. Announcements should
be idempotent by database identity; descriptors need a route usable by the
receiving client, not merely an absolute path on the creator's machine.

## Decisions to retain

- **Known is not automatically selected.** Discovery removes configuration
  repetition; existing scope/activation policy still determines which stores are
  read, deliver core, or participate in background work.
- A descriptor does not convey credentials. Verify identity on connection, and
  keep stale routes distinct from deleted stores.
- Proposed failure behavior: store creation succeeds if announcement fails and
  reports registration pending, with an explicit retry path. Do not claim the
  announcement succeeded or quietly require the registry to be online to create.
- Determine how a creating client finds its bootstrap owner, which descriptors
  the bootstrap owner accepts, and how route updates/withdrawals work. The global
  store need not be opened by another process to register a descriptor.

First inspect the existing library catalog and enrollment paths. Prototype
announcement and one-bootstrap discovery before adding elaborate propagation,
filesystem search, or automatic integration enrollment.

## Existing pieces and the actual gap

Project setup already records the verified owner ID in the **local** library via
`register_local_project`; it does not announce to the global owner. The library
already has descriptor revisions, withdrawal, idempotent announcements, pending
work, and read-only `library_catalog` / `known_sources()`.

The missing connection is create → seed-owner registration → seed-only client
resolution. Native `databases` currently describes that process's startup stores,
not a mutable external-store directory. Existing peer announcements intentionally
omit endpoint routes, so their catalog alone cannot provide “configure once.”

Reuse the descriptor/revision/withdrawal model without coupling this to snapshot
publication or peer replication. Keep the directory as owner-held metadata, not
ordinary memory nodes. A seed reference can point at local global-owner config;
across devices it must resolve to an actual connection route. Client-local
explicit overrides should win, and project activation needs device-appropriate
bindings rather than treating one machine's absolute paths as universal.

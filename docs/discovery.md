# Discover stores from one seed

A seed owner can publish configuration for itself and other owners, including
further discovery links. Clients combine those sparse declarations by canonical
database ID, apply explicit local overrides, then select stores under the existing
workspace and owner-capability boundaries. Discovery reads **configuration
metadata**, not memories, and never starts an owner or opens a remote path locally.

This is explicit operator-authored publication. Creating a store does not yet
announce it to a seed automatically. Library snapshots and replication remain
separate mechanisms.

## Configure the client

Use the ordinary [declarative configuration](declarative-config.md) file:

```toml
schema = "mneme.config.v1"

[stores.seed]
id = "01ARZ3NDEKTSV4RRFFQ69G5FA1"
owner = { url = "https://memory.example.test/mcp", db = "user" }
discovery = true
```

Replace the example ID and route with verified owner bindings. A seed needs its
canonical ID and a complete route; SSH routes also need `ssh_mcp_port`. Discovery
is opt-in. An ID-only local entry can override an imported store but is not an
acquisition seed by itself.

To override imported policy, add only the fields you mean to change:

```toml
[stores.work]
id = "01ARZ3NDEKTSV4RRFFQ69G5FA2"
permissions = { write = false }
background = { maintenance = false }
```

The local label `work` belongs to that ID even if a publisher calls it `project`.
Local explicit values win; omitted values may be filled remotely. Explicit
`false` and `[]` are not omissions. To remove a nullable setting use, for example,
`clear = ["cache", "retrieval", "guidance.tag_guide"]`; do not also set that field.

Imported per-store activation, permissions, delivery, applicability, capture and
background switches are real policy, not decorative hints. Omitted grants use the
same defaults as deliberately declared local stores. Top-level process settings,
library definitions and workspace privacy controls remain local. Exclusions,
isolated-mode allowlists, disabled entries and native owner capabilities still
win. Explicit client-local `active.enabled = false` also denies catalog acquisition; it does not mean
“catalog-only.” Default project activation without matching roots does not itself
prevent catalog contact.

Names are labels, not identities. Conflicting imported labels get generated names
rather than choosing a winner. Bind a local name to an ID when using it in
`read_default` or an allowlist. A missing or ambiguous name does not gain a fallback
grant. Without `read_default`, the ordinary `project-or-misc` rule applies;
`workspace = "any"` does not automatically become the default.

## Publish an owner catalog

Write a separate TOML file on the owner host:

```toml
schema = "mneme.discovery-catalog.v1"

[stores.personal]
id = "01ARZ3NDEKTSV4RRFFQ69G5FA1"
active = { workspace = "any" }
background = { maintenance = false }

[stores.project]
id = "01ARZ3NDEKTSV4RRFFQ69G5FA2"
owner = { url = "https://project.example.test/mcp", db = "project" }
discovery = true
permissions = { read = true, write = true }

[stores.project.devices.laptop.active]
roots = ["/absolute/project"]
```

This example supplies the seed's own activation settings and refers to a second
publisher. The second owner's `discovery = true` requests its catalog too; omit it
when that owner has no further publication.

Start the existing native owner with a publication bound to its database alias:

```sh
mneme-mcp --db user=/absolute/store.db \
  --discovery-catalog user=/absolute/discovery.toml
```

Use the owner's existing backend, profile and transport options as appropriate;
this is not a request to create another process over an already leased store.
`--discovery-catalog` may repeat for distinct owner aliases. It is unavailable on
configured/library/router coordinators, which are not implicit publishers.

The owner validates publication files before opening stores, rereads the bounded
file for each catalog request, and supplies its verified publisher ID and content
hash. It does not require a database checkout or cold-operation capability.
Publication remains available while an owner is released, but is fenced to the
original admitted identity across release/resume. Missing publication, malformed
content and identity/revision mismatch are errors, not empty catalogs.

Catalog files contain only `schema` and `stores`; do not hand-author the wire
result's `publisher_db_id` or `catalog_sha256`. The content hash covers normalized
store declarations, not TOML comments or whitespace. For external clients,
`discovery_catalog` requires `db` and canonical `expected_db_id`, with optional
`expected_catalog_sha256`. Updated native clients check its advertised contract;
unsupported servers do not fall back to a guessed operation.

### Devices, routes and secrets

Set `device = "laptop"` at the top of the client file to accept the matching
`devices.laptop` path patch above. Remote absolute workspace paths must be
qualified by device. Off-device omissions do not block a matching deeper
contributor; unresolved required device scope still blocks activation. Ordinary
local path overrides need no device qualifier.

A remote route is a coherent bundle, not a bag of interchangeable fragments.
Publish different complete routes for clients on different devices when necessary:

```toml
[stores.project.devices.laptop]
owner = { url = "http://127.0.0.1:18767/", db = "project" }

[stores.project.devices.desktop]
owner = { url = "ssh://operator@owner.example.test", db = "project", ssh_mcp_port = 18766 }
```

The selected device's `owner` replaces the whole shared `stores.project.owner`
bundle before branch merging; omitted optional fields do not leak in from the
shared route. Without a matching device route, the shared route remains the
default. A device-only route has no off-device fallback. Device routes require
both `url` and `db`, and SSH still requires `ssh_mcp_port`. Explicit local owner
overrides remain stronger than imported device routes. Local device routes can
also supply a discovery seed.

Local route fields may refine an unambiguous imported route. Catalogs cannot name
`token_env` or contain credentials, including in off-device routes; a local secret
reference needs a complete local route/identity binding. An owner-side `path` is
identity evidence, never a local-open instruction. Discovery imports no executable
configuration.

## Branches, conflicts and cycles

For each target ID and setting, the first publisher declaring it on a branch
wins over deeper publishers on **that branch**. Omitted fields let deeper values
through. Independent branches merge, regardless of their lengths:

```text
A ── B ───── D
└── C ── E ─ D
```

B's and E's declarations for D are peers. B does not win because it is nearer A.
If A declares that same D setting, it shadows both. If a deeper publisher has an
unblocked route through another branch, that contribution survives. Cycles use
visited sets; network response order is not configuration policy.

Equal values agree and disjoint fields fill gaps. Conflicting boolean switches
resolve conservatively to false with a diagnostic. Other conflicts block the
store rather than dropping the field and accidentally regaining a default grant.
Local explicit values resolve the corresponding imported conflict.

A reachable opt-in can authorize catalog-only contact even if a later merge
turns final discovery or activation off. Acquired facts remain visible for the
run; this avoids recursive policy oscillation. Inspection distinguishes metadata
contact from final memory-operation eligibility. An unanswered publisher is
incomplete evidence, not an empty one.

## Inspect, refresh and replay

```sh
# Initial acquisition if needed, otherwise reuse the retained receipt.
mnemed --json config select --config /absolute/memory.toml --cwd /absolute/project

# Explain retained values without contacting publishers.
mnemed config inspect --config /absolute/memory.toml --cwd /absolute/project --offline
mnemed --json config inspect --config /absolute/memory.toml --cwd /absolute/project --offline

# Explicitly acquire current catalogs and explain the result.
mnemed config inspect --config /absolute/memory.toml --cwd /absolute/project --refresh

# Replay only the exact prior selection. Never fetches new policy.
mnemed --json config select --config /absolute/memory.toml --cwd /absolute/project \
  --expected-selection-digest SELECTION_DIGEST

# Interactive provenance, even without a readable default.
mnemed --memory-config /absolute/memory.toml tui --view config
```

`inspect` reports effective values, sources, local overrides, contributing branch
witnesses, shadowed declarations, conflicts and unavailable publishers. Its JSON
schema is `mneme.config-inspection.v1`. It exits successfully for a diagnostic
report even when selection fails: check `error`, `selection_digest`, `cache_status`,
`report` and any separate refresh `attempt`. `select` remains strict. A failed
explicit `select --refresh` refuses instead of silently presenting old policy as
fresh.

Without `--offline`, inspection may acquire catalogs if there is no retained
receipt. TUI config startup uses that same workflow, but does not start memory
reads merely to show configuration. Press `C` from a normal view/help, or `c` in
the source picker. Closing the startup inspector returns to the picker; choose a
source explicitly to open its memories. See [Observatory](observatory.md).

Receipts live under `$XDG_CACHE_HOME/mneme/discovery`, falling back to
`$HOME/.cache/mneme/discovery`. They bind the exact local sources, selection request,
workspace/privacy context and acquired graph. Keep the same config path, cwd,
profile and environment when replaying a selection. Local source edits, including
comments, change that boundary; identical catalog content can retain its hash.

Ordinary reuse is frozen, not an implicit refresh. A failed refresh retains the
previous usable receipt and reports stale coverage plus the failed attempt.
Normal selection or offline inspection can still use that receipt. A first
incomplete acquisition creates no usable selection. Complete new conflicts may
replace old policy and remove its grants. Changed receipt bytes or local
boundaries invalidate old selections; reselect and reassess queued work. Do not
rewrite a stale digest or copy old permissions into a new snapshot.

This deliberately does not promise immediate remote-policy revocation. Refresh
explicitly, then restart configured MCP processes/start fresh sessions. Hooks and
native binaries must be updated as a matching bundle: child work and revalidation
use expected-digest offline replay, including local-only configurations. Older
native binaries refuse unsupported replay flags or discovery fields; helpers do
not fall back to fresh acquisition.

## Bounds and validation boundary

Default discovery budget is 15 seconds and 1 MiB of aggregate native receive data,
including initialization and failures. `select`/`inspect` accept `--timeout-ms`
(up to 30,000) and `--max-native-bytes` (up to 16 MiB); structural limits still
apply. Catalogs are bounded to 64 KiB each, 64 publishers and 1 MiB aggregate
catalog content. Receipt and provenance limits are 4 MiB and 1 MiB respectively;
omitted presentation detail is counted without changing resolution. Refreshes
serialize through a bounded cache lock and publish atomically.

This feature changes configuration, not the memory database format. Source and
disposable tests do not deploy it. Update each configuration-reading native
client and matching helpers before enabling discovery on a real device; no
service, hook, enrollment or live-store change is implicit in a config example.

# Native declarative memory configuration

This crate owns **metadata selection**, not owner startup, network calls, database
opens, ranking or capture inference. CLI, MCP and hooks consume the same typed
selection. A configured-but-unavailable owner stays configured; it never becomes
miscellaneous work.

Internally, `selection` owns admission, activation and operation eligibility;
`workspace` owns bounded filesystem collection and frozen-request revalidation;
`discovery` owns sparse declarations and pure catalog-graph resolution. The private privacy ordering and store-disposition types keep those
policy decisions coherent without changing the public configuration/snapshot DTOs.
A digest witnesses metadata continuity; it does not mint owner-side authority.

The normal user file is `$XDG_CONFIG_HOME/mneme/memory.toml`, or
`~/.config/mneme/memory.toml` on Unix. Windows falls back to
`%APPDATA%/mneme/memory.toml`, then `%USERPROFILE%/AppData/Roaming/mneme/memory.toml`.
An optional nearest `.mneme/memory.toml` uses the same schema.

## Deliberate-store defaults

For an intentionally declared store, omitted read/write grants, core/context
switches and recall/record/maintenance switches are **true**. Top-level background
is enabled with model `gpt-6.1-sol`, effort `medium`. These defaults do not imply store
discovery: a local entry or explicitly acquired catalog must declare the owner,
and parsing never contacts or starts anything.
Activation defaults to `workspace = "project"`; declare the activation appropriate
to each owner. Project overlay entries without roots receive the overlay project
root. User-config project entries must have roots to activate.

Core and foreground context do not depend on background enablement. Background
recall needs read + context + recall; recording needs write + record; maintenance
needs read + write + maintenance. Read/write grants also bound explicit operations.
The owner can refuse further: these are upper bounds, not minted native capability.

## A recipe, not special names

Names below are ordinary labels. Activation metadata supplies all behavior.
Replace the example canonical IDs and guide IDs with verified IDs from your owners.
Do not paste secrets into the file; `token_env` names an external environment variable.

```toml
schema = "mneme.config.v1"
read_default = "project-or-misc"

[background]
enabled = true

[stores.personal]
id = "01ARZ3NDEKTSV4RRFFQ69G5FA1"
owner = { url = "ssh://memory@example.test", db = "user", ssh_mcp_port = 19001 }
active = { workspace = "any" }
background = { maintenance = false }
retrieval = { tags = ["collaboration-preference"] }
# Preserve the preference-only automatic capture contract explicitly.
capture = { kinds = ["lesson"], evidence_kinds = ["user_statement"], required_tags = ["collaboration-preference"], allow_associations = false, max_summary_bytes = 384, max_body_bytes = 1024 }
# Startup core everywhere; automatic recording/task context only in this project.
applicability.context = { roots = ["/absolute/project"] }
applicability.record = { roots = ["/absolute/project"] }

[stores.ordinary]
id = "01ARZ3NDEKTSV4RRFFQ69G5FA3"
owner = { url = "ssh://memory@example.test", db = "project", ssh_mcp_port = 19002 }
active = { workspace = "no-project", excluded_roots = ["/absolute/private-work"] }
delivery = { core = false }

[stores.work]
id = "01ARZ3NDEKTSV4RRFFQ69G5FA2"
owner = { url = "http://127.0.0.1:19000", db = "project" }
active = { workspace = "project", roots = ["/absolute/project"] }
guidance = { tag_guide = { id = "01ARZ3NDEKTSV4RRFFQ69G5FA4", content = "summary" } }
cache = { library = "reference", mode = "owner-first" }

[libraries.reference]
catalog = "/absolute/library/catalog.json"
replicas.remote = { url = "ssh://memory@example.test", ssh_mcp_port = 19003 }
```

An owner-side `owner.path` is optional identity evidence, never a local-open target.
SSH transport ports (`ssh://memory@example.test:2222`) and `ssh_mcp_port` are distinct.
Library cache resolution uses the store's canonical ID; aliases, generations and
project hashes are not repeated in human config. Explicit cross-project library
reads use an admitted configured library, not automatic activation of every entry.

## Explicit catalog discovery

A client may acquire recursive **configuration metadata** from an identity-checked
seed. Native owner publication is separately configured; it is not a memory node,
library snapshot, database write or permission to start an owner.

```toml
schema = "mneme.config.v1"
# Required only when accepting device-qualified workspace paths.
device = "laptop"

[stores.seed]
id = "01ARZ3NDEKTSV4RRFFQ69G5FA1"
owner = { url = "https://memory.example.test/mcp", db = "user" }
discovery = true

# Canonical identity, not label, selects the imported declaration to override.
[stores.work]
id = "01ARZ3NDEKTSV4RRFFQ69G5FA2"
permissions = { write = false }
```

Operator publication metadata uses a distinct closed schema:

```toml
schema = "mneme.discovery-catalog.v1"

[stores.project]
id = "01ARZ3NDEKTSV4RRFFQ69G5FA2"
owner = { url = "https://project.example.test/mcp", db = "project" }
discovery = true
permissions = { read = true, write = true }
[stores.project.devices.laptop.active]
roots = ["/absolute/project"]
```

Sparse values remain sparse until resolution. Omission is no opinion; explicit
`false`, `[]` and `clear = ["cache", "retrieval", "guidance.tag_guide"]` remain
meaningful. Clears only accept those nullable fields and cannot also set the same
field. Local explicit values override by canonical ID. Remote owner routes are
atomic bundles; local route leaves may refine them only if the resulting bundle
is valid and unambiguous. A local `token_env` requires a complete local URL and
owner database selector; publications cannot name local secrets.

For each store setting, the first declaring publisher on a branch is a barrier.
Independent branches contribute even at different depths; a bypass can preserve
a deeper contribution. Equal values agree, conflicting booleans meet at false,
and other conflicts block the store rather than regain defaults. Unknown or
unanswered publishers are incomplete evidence, not empty catalogs. Discovery
metadata contact and final activation are reported separately.

Publications cannot import universal absolute workspace paths. Put these under
`devices.<client-device>`; only the exact client-selected patch applies. An
unmatched device-scoped store is blocked unless the needed scope has an explicit
local replacement. Ordinary local paths need no device qualification. Libraries,
process configuration and secret references are not remotely imported.

`prepare_workspace` collects local declarations plus the complete existing
privacy-boundary witness before acquisition. A native client acquires bounded
catalogs and atomically publishes a `DiscoveryReceipt` beneath
`$XDG_CACHE_HOME/mneme/discovery` (or `~/.cache/mneme/discovery`).
`finish_workspace` verifies that receipt and performs pure resolution/selection.
`resolve_discovery` and `WorkspaceDeclarations::resolve_graph` expose provenance,
conflicts and coverage without memory reads. This crate performs no network I/O.

Discovery snapshots add only an optional compact receipt binding and local-boundary
hash. `revalidate_exact` reloads the pinned receipt and exact local metadata, then
reruns resolution offline; changed receipt bytes or local privacy boundaries
refuse old work. A refresh is an explicit client action, not replay. No-discovery
snapshots retain their existing serialized shape and digest. Older clients reject
the new syntax through closed-field parsing rather than silently ignore policy.

## Narrowing and boundaries

Project overlays add disjoint store/library names, never replace a user owner.
They can reference user libraries and allowlist entries. Background enabled flags
combine with AND; model/effort remain user-owned. Overlay workspace profiles can
only become stricter (`default < private < isolated`), and allowlists intersect.

`[workspace]` accepts `profile`, `allow_stores` and `allow_libraries`. Omitted
allowlists leave normal/private admission unconstrained; an explicit empty list
admits nothing. Isolated mode requires explicit allowlists, including for deliberate
non-active selection. Active profiles default to `default` and `private`; explicit
isolated allowlisting admits only those selected names. Exclusions still win.

All operations support optional independent `applicability.{core,context,recall,
record,maintenance}` root/exclusion predicates. Empty operation roots mean no
additional restriction, preserving broad core/manual access. Nonempty positive
operation roots admit work in the explicitly selected project, not in nested
`.git`, `.mneme` or configured routing boundaries underneath it. An explicitly
listed child root can opt that child back in. Both lexical and canonical ancestry
are fenced, including loopback aliases into the parent root. `permissions.read=false` automatically denies reads/core/
context/recall/maintenance without requiring changes to other defaults; the store
can remain a write-only recording destination. Such a destination cannot fetch
its node-backed tag guide or derive associations from unread memories.

Any project `.mneme` presence, configured native project root, explicit privacy
profile or recognized Codex routing binding prevents misc fallback. Malformed
metadata refuses rather than silently falling through. Nested `.git` or `.mneme`
boundaries fence inherited project activation. Lexical and canonical ancestry
both participate: positive root admission must contain both; exclusion of either
wins. Retained profile v1/v2 boundaries are recognized; a selected legacy
`library_config` is preserved as an explicit adapter boundary, not translated into
an ambient native library grant.

## Frozen contract

`select_workspace` loads bounded regular files and returns
`mneme.config-selection.v1`. The snapshot freezes raw-file SHA-256 witnesses,
fully resolved typed config, lexical/canonical origins, profile, project boundaries,
store inventory, operation eligibility, admitted libraries and default selection.
Product snapshots also freeze the original selection request: explicit/discovery
mode, the exact user path even when absent, original lexical cwd, requested
profile and accepted lexical/device-exemption witnesses. Its digest hashes the
complete deterministic JSON snapshot with `digest=""`.

Declarative user and project TOML may be symlinks, including relative/chained
links managed by Home Manager. Their resolved targets must be readable regular
files within the same 64 KiB bound. Broken/looping links, directories, FIFOs and
oversized targets refuse; they never count as absent configuration. The logical
project location supplies overlay scope, not the target's directory. Legacy
privacy/routing metadata and the project `.mneme` directory retain their stricter
non-symlink admission; database, body and lease rules are unchanged.

Snapshots retain the logical request path and canonical target witnesses.
Retargeting a config link invalidates an existing selection even when the new
target has identical bytes. Restart configured MCP processes/start fresh sessions
after a Home Manager generation switch; queued work must be reselected and
reassessed, never silently redirected.

A digest is an integrity/staleness witness, **not authentication or write authority**.
Consumers call `revalidate_exact` before native contact and before applying frozen
work. The helper reselects the same request, detecting newly created configs and
privacy boundaries as well as edits/deletions; it refuses stale admission rather
than replacing the original binding. Pure programmatic `select` snapshots have no
filesystem request: they are explicitly immutable caller-owned in-memory metadata,
with no filesystem continuity claim. External product entry points always freeze
a request and do not accept arbitrary caller-supplied snapshots.
No fetched guide text or environment-secret value is included. A discovery receipt
pins catalog availability/coverage at selection time, not instantaneous remote
policy revocation. Full provenance is exposed by inspection, not copied into every
control-plane snapshot.
The optional `guidance.tag_guide` selects an exact same-owner semantic node's
complete canonical summary, fetched only after activation, operation admission
and native identity verification.

`.mneme/hippocampus.md` remains the existing background hippocampus thread
instruction file, analogous to `AGENTS.md` for that thread. It is not migrated to
a database node or injected into the foreground agent. Existing project-focus
file admission and revision guards remain in the integration; node-backed
hippocampus selection and file overrides are deferred.

Input is capped at 64 KiB per configuration/routing file, 256 KiB aggregate
workspace metadata, 64 ancestry steps per spelling, 64 stores, 32 libraries and
16 replica endpoints per library. Resolved snapshots are capped at 512 KiB.
Unknown fields, duplicate TOML/JSON keys, duplicate names/native IDs, noncanonical
IDs, credential-bearing endpoints and invalid/overlarge policy combinations refuse.
Parser errors never echo offending source text.

Discovery catalogs are at most 64 KiB each, 64 publisher snapshots and 1 MiB
aggregate catalog content. Graph edges/declarations derive from the 64-by-64
publisher/store envelope. Receipts are at most 4 MiB, and inspection reports at
most 1 MiB; omitted report detail is counted explicitly without removing conflict
or blocked-store markers. Receipt files and their containing cache directory must
be owned by the current user and not writable by others on Unix.

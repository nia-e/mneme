# One memory configuration

`memory.toml` describes databases, their canonical connections, workspace
activation, read/write ceilings and delivery/background switches. The native
selector owns these rules; CLI, configured MCP and Codex hooks consume the same
selection. Generated adapter JSON is machinery, not another store-policy file.

## A small explicit recipe

The names `global`, `project` and `misc` are ordinary entries, not reserved database
classes. Replace the example IDs/endpoints with verified native owner bindings.
Loading or selecting this configuration does not start or contact owners.

```toml
schema = "mneme.config.v1"
read_default = "project-or-misc"

[background]
enabled = true
model = "gpt-6.1-sol"
effort = "medium"

[stores.global]
id = "01ARZ3NDEKTSV4RRFFQ69G5FA1"
owner = { url = "ssh://memory@example.test", db = "user", ssh_mcp_port = 19001 }
active = { workspace = "any", profiles = ["default", "private"] }
permissions = { read = true, write = true }
delivery = { core = true, context = true }
background = { recall = true, record = true, maintenance = false }

[stores.project]
id = "01ARZ3NDEKTSV4RRFFQ69G5FA2"
owner = { url = "http://127.0.0.1:19000/", db = "project" }
active = { workspace = "project", roots = ["/absolute/project"], profiles = ["default", "private"] }
permissions = { read = true, write = true }
delivery = { core = true, context = true }
background = { recall = true, record = true, maintenance = true }

[stores.misc]
id = "01ARZ3NDEKTSV4RRFFQ69G5FA3"
owner = { url = "ssh://memory@example.test", db = "project", ssh_mcp_port = 19002 }
active = { workspace = "no-project", profiles = ["default", "private"], excluded_roots = ["/absolute/private-work"] }
permissions = { read = true, write = true }
delivery = { core = false, context = true }
background = { recall = true, record = true, maintenance = true }
```

A project's `.mneme/memory.toml` uses the same schema. Its explicitly scoped stores
are added to the user configuration; duplicate names/identities refuse rather than
silently replacing a user binding. Process/background settings remain user-owned.
A project entry in an overlay can omit `active.roots`: its containing project root
supplies the scope. Disabled project presence still prevents `misc` fallback.

Both user and project TOML files may be symlinks, including Home Manager's
relative/chained links. The target must be a readable regular file of at most
64 KiB. Broken links, loops, directories, FIFOs and oversized files fail closed,
not as missing configuration. The overlay's project directory still supplies its
scope even when the file itself lives in `/nix/store`. This does not permit
symlinked project `.mneme` directories, legacy privacy metadata, databases, bodies
or leases.

A configured process freezes the logical config path, resolved target and file
contents. Changing the target (even to identical contents in a new Home Manager
generation) invalidates that selection: restart configured MCP processes/start
fresh sessions and reassess queued work after activating a new generation. A
symlink is not a live hot-reload mechanism.

Be deliberate about grants: omitted permissions, delivery and per-store background
switches have enabled defaults. A minimal example is not a scope-preserving
migration of an older installation.

### Independent controls

- `permissions.read = false` blocks reads; `write = false` blocks writes and upkeep.
- `delivery.core` controls passive startup core independently of background work.
- `delivery.context` controls task context. `background.recall` enables its
  asynchronous collection; `background.record` enables automatic recording.
- `[background].enabled = false` disables paid/background work, not manual reads
  or passive core. Per-store flags narrow native grants; they cannot expand them.
- `[workspace] profile = "isolated"` admits only explicitly allowlisted stores and
  libraries. Exclusions and disabled entries do not become fallback permission.

For narrower uses of an otherwise active owner, operation applicability keeps one
canonical identity rather than creating duplicate aliases. For example, to keep
personal core available broadly but automatic personal recording project-only:

```toml
[stores.global.applicability.record]
roots = ["/absolute/project"]
```

The same scoped table exists for `core`, `context`, `recall` and `maintenance`, with
`roots` and `excluded_roots`. A migration must preserve each existing operation's
scope, not broaden global auto-write because the name sounds global.

## Background instructions and tag guides

Keep the existing project `.mneme/hippocampus.md` **as-is**. It remains instructions
for the background thread, using the current project-focus convention. Adopting
`memory.toml` does not copy that text into a database, rewrite/delete the file or
render it as foreground core. Database-backed hippocampus/purpose guidance is
deferred; do not add a `guidance.hippocampus` binding.

The existing tag-guide node feature is separate and remains available:

```toml
[stores.project.guidance]
tag_guide = { id = "01ARZ3NDEKTSV4RRFFQ69G5FA5", content = "summary" }
```

This illustrative ID must be replaced with the deliberately selected same-owner
semantic node. A tag guide uses its complete canonical summary for topic proposals
and guarded maintenance; it is not the background thread's hippocampus
instructions, a permission grant or automatic foreground core. Missing, partial
or unavailable tag guidance defers dependent tagging/upkeep, not ordinary reads.

## Inspect before publication

```sh
mnemed --json config select --config /absolute/memory.toml --cwd /absolute/project
mneme-mcp --memory-config /absolute/memory.toml
```

Ordinary CLI commands use `--memory-config PATH` (not the legacy library/TUI
`--config` flag). `--store NAME` deliberately selects an admitted configured store,
including an inactive one, without changing automatic activation. Profile and
exclusion boundaries still apply. `--user` refuses under declarative configuration:
store names are not hard-coded global/project roles.

Selection is metadata-only, bounded and digest-qualified. It reports configured
inventory separately from active stores and operation eligibility. A configured
MCP process captures its startup working directory when `--cwd` is omitted. Its
selection stays bound to that workspace and is exactly revalidated on subsequent
calls; it does not silently hot-switch when configuration changes. A launcher that
preserves session cwd, as verified for the local Codex app, can register one server
globally without per-project installation. Use `--cwd WORKSPACE` when a launcher
does not preserve the intended workspace. A workspace/configuration change needs
a fresh process/session, not a guessed fallback.

Native direct owner connections support HTTP and SSH. Optional libraries and cache
references reuse the existing catalog, immutable generations and publisher state;
replicas are not extra writable stores. The same configured MCP server exposes
`library_catalog`, `library_query`, `library_recall_context` and `library_get` for
admitted collections. Pass a `library` name when more than one is admitted; the
sole collection is the default otherwise. A separate `--library NAME` process is
an optional read-only collection-only view, not a second required registration.
Preserve existing catalogs, publication state and immutable snapshot identities
when adopting configuration; a snapshot witness cannot become a live-owner write
guard.

For Codex, one global registration can consume the one native configuration:

```toml
[mcp_servers.mneme]
command = "/runtime/bin/mneme-mcp"
args = ["--memory-config", "/absolute/memory.toml"]
```

Hook trust and automatic recording remain separate explicit setup steps. Do not
keep old direct owner registrations alongside this server as an accidental grant
bypass. Existing project memory entries in TOML select the project; they do not
require an additional project MCP process or project hook installation.

## Prepare one Codex device actor

Use Python 3.11+ and a matched rebuilt native/helper bundle. This command prints a
reviewable plan; it does not publish config, grant hook trust, start a service or
contact a database:

```sh
python3 /runtime/lib/registry_config.py prepare-native \
  --memory-config /absolute/memory.toml --native-cli /runtime/bin/mnemed \
  --cwd /absolute/project --state-dir /absolute/retained-device-state \
  --reader-codex /absolute/pinned/codex --hooks-program /runtime/lib/hook_launcher.py \
  --hook-config-path /absolute/generated-hooks.json
```

The resulting hook adapter holds only native config/executable paths, retained
state directories and pinned Codex binaries/auth paths. `--workspace-adapters`
accepts an explicit JSON list of `{root,state_dir,reader_codex}` machinery bindings;
preparation pins the reader hashes. This preserves project-specific reader versions
and existing ledgers under one device actor. It does not select stores or grant
background authority. Core and asynchronous lifecycle recipes are emitted
independently and consult native configuration when invoked.

An optional project-scoped migration/package route,
`install.py prepare --memory-config PATH`, stages the
configured MCP registration and emits **no project hooks**. Supply the explicit
native binaries, project root, absent immutable prefix and reader executable; do
not also supply old registry/model/recording policy flags. An explicitly pinned
`--existing-service-config` preserves the old owner's configuration/state and binary
without invoking it. Staged configured MCP and retained owner binaries may differ;
both are independently pinned. Install preparation/apply never re-enrolls owners.
This route is not required when one globally registered configured server inherits
the session workspace.

Before replacing hooks, disable future old invocations, close old sessions, and
check every retained state directory with `registry_config.py quiescence`. A quiet
snapshot while old hooks can still spawn is not quiescence. Keep paid usage,
uncertain outcomes, queues and receipts; never reset them to make migration pass.
Use fresh sessions after publication, and replace old direct owner registrations
rather than leaving a silent configured-permission bypass beside the new route.

Source tests do not update an installed binary's embedded helper bundle. Rebuild
and verify copied helper hashes, finite disposable smoke and native identities
before any live cutover. Configuration-only migration keeps databases, bodies,
owner PIDs, embedding caches, library publication identities and ledgers intact.
Actual database/format migration needs its own paired backup and recovery plan.

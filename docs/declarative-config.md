# One memory configuration

`memory.toml` describes databases, their canonical connections, workspace
activation, read/write ceilings and delivery/background switches. The native
selector owns these rules; CLI, configured MCP and Codex hooks consume the same
selection. Generated adapter JSON is machinery, not another store-policy file.

## Which JSON files still matter?

The extension does not tell you whether a file is configuration, generated
machinery or retained state:

| File or role | What to do with it |
| --- | --- |
| `memory.toml` and project overlays | Author store connections, selection and permissions here for the native path. |
| Hook adapter JSON, managed `service.json` and client/owner connection files | Keep while a hook, launcher or service references them. They supply machinery bindings; some older or custom integrations still need their own connection configuration. |
| Codex/Claude host settings | Required by the host's integration format. Native TOML does not replace hook or MCP registration. |
| Job/packet ledgers, capture receipts and queued work | Retained operational state, not editable config clutter. Receipts can prevent replay and ledgers preserve paid-work accounting. |
| Old plans, install receipts and backups | Historical/recovery evidence. An unused file is not automatically safe to delete; establish current references and recovery needs first. |

A native recipe is not proof that an older custom hook has been replaced. Check
the actual hook/MCP/service commands and their referenced files before removing
duplicate-looking JSON. Moving policy to TOML should reduce hand-maintained
configuration, not erase another integration's scope or replay history.

## A small explicit recipe

The names `global`, `project` and `misc` are ordinary entries, not reserved database
classes. Replace the example IDs/endpoints with verified native owner bindings.
Loading or selecting this local-only recipe does not start or contact owners.
Opt-in [store discovery](discovery.md) can instead acquire bounded configuration
catalogs from explicitly delegated owners; it still never starts them.

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

[stores.global.capture]
intent = "cross-workspace"

# Broad reads do not require automatic export from private project work.
[stores.global.applicability.record]
excluded_roots = ["/absolute/private-work"]

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

Be deliberate about grants: omitted permissions, core/context delivery and
per-store background switches have enabled defaults. Contradiction notices are
off by default. A minimal example is not a scope-preserving migration of an older
installation.

### Independent controls

- `permissions.read = false` blocks reads; `write = false` blocks writes and upkeep.
- `delivery.core` controls passive startup core independently of background work.
- `delivery.context` controls task context. `background.recall` enables its
  asynchronous collection; `background.record` enables automatic recording.
- `delivery.contradiction_notice = true` opts one owner into a brief unresolved-
  contradiction notice in the session-scoped startup core hook, at most once per
  owner/session across startup, resume and compact. It defaults to `false` and
  requires that owner's existing core/read eligibility, not background recording.
  The standalone core hook's `--config` mode does not issue these notices. The
  preference grants no additional owner access.
- `[background].enabled = false` disables paid/background work, not manual reads
  or passive core. Per-store flags narrow native grants; they cannot expand them.
- `[workspace] profile = "isolated"` admits only explicitly allowlisted stores and
  libraries. Exclusions and disabled entries do not become fallback permission.

Per-owner `background.core_cleanup = true` (the default) permits trusted,
meaning-preserving cleanup on existing core notes within enabled maintenance.
Today this is ordinary-topic tag upkeep that preserves all protected tags; it does
not add a background body/summary editor. Set `core_cleanup = false` in the
store's `background` table to keep core-target cleanup in the foreground while
ordinary non-core maintenance continues. A small edit is not necessarily a
meaning-preserving edit; this is trusted classification, not a semantic proof.
Core creation, membership changes, meaning changes, destructive adjudication and
episode corrections remain foreground decisions. The switch does not grant
operator capability or change explicit foreground authority.

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

### Explicit project roots and shared parent scopes

For a deliberately shared project across a directory of worktrees or repositories,
replace its activation table with:

```toml
[stores.project.active]
workspace = "project"
subtree_roots = ["/worktrees/mneme"]
excluded_roots = ["/worktrees/mneme/private"]
```

`subtree_roots` includes the parent and everything below it, including nested
`.git` directories, Git worktree `.git` files, `.mneme` projects, and newly created
unrelated repositories. This is an explicit shared scope, not Git-family detection
or worktree auto-discovery. Selection checks the current workspace; it never walks
the parent's children or freezes a discovered membership list. There is no glob
expansion.

Existing `roots` behavior is unchanged: for `workspace = "project"` activation and
operation applicability, ordinary directories below a root are included but nested
project boundaries stop inheritance unless the child is explicitly listed.
`workspace = "any"` activation retains its existing broader root-prefix behavior;
its operation applicability is still checked independently. Use `subtree_roots`
when you mean to include nested projects, rather than changing workspace modes as
a workaround. A scope can mix `roots` for individual projects and `subtree_roots`
for broad parents; these are alternative positive grants, not intersecting filters.

All five operation-applicability tables also accept `subtree_roots`. For example,
keep broad personal reads while limiting automatic recording to the shared tree:

```toml
[stores.global.applicability.record]
subtree_roots = ["/worktrees/mneme"]
excluded_roots = ["/worktrees/mneme/private"]
```

Exclusions still win, including lexical and canonical aliases. Every workspace
path witness must be covered by a positive grant; a symlink inside an allowed tree
does not implicitly admit an outside target. A shared scope does not bypass
permissions, disabled entries, profiles, isolated allowlists, or nested overlay and
privacy metadata. A scope with neither positive list remains unrestricted by roots;
a nonempty `subtree_roots` list restricts it even when `roots` is empty.

Within each activation or operation scope, `roots` and `subtree_roots` share one
budget: **256 paths and 16,384 total UTF-8 path bytes**, with no duplicates across
the two lists. `excluded_roots` has a separate budget of the same size. Each path
must be a normalized absolute path of at most 4,096 bytes. This preserves the
path-data envelope while allowing up to four times the old 64-path candidate
count. The 64-store, 64 KiB per-file, 256 KiB aggregate configuration-source and
512 KiB selection-snapshot limits remain unchanged.

Older native clients reject `subtree_roots` as an unknown field rather than
silently ignoring its meaning; they also reject lists longer than 64 paths.
Configurations without the new field and with at most 64 paths remain accepted.
Update each configuration-reading native client before publishing this syntax,
then restart configured processes/start fresh sessions. Scope changes invalidate
frozen selections; new metadata on the current workspace's ancestry still requires
reselection even when the shared scope admits the new project. Creating an
unrelated sibling project does not invalidate another workspace's selection. This
is a configuration-admission change, not a database migration; source tests alone
do not update installed clients.

### What automatic recording is for

`capture.intent` is independent of workspace scope. Its default, `"ordinary"`,
retains ordinary source-bound lessons, episodes and possibilities within the
configured capture limits. Neither a store named `global` nor a required tag
implicitly changes that intent.

Use `"cross-workspace"` for knowledge whose usefulness extends beyond the source
workspace: reusable technical lessons, collaboration preferences, and qualified
experiences or open questions. Ordinary capture kinds, evidence, bounds and
same-owner associations remain available. Select by applicability, not by where
the turn happened or whether another destination is missing. Project architecture,
local decisions and task state belong in their admitted project/misc destination;
general lessons retain decisive source, revision and uncertainty qualifications.
Private or embargoed project/security details are not exportable merely because
they might be reusable. Use operation exclusions for a deterministic no-record
boundary, as above; the assessor's semantic scope judgment remains fallible.

Use `"collaboration-preferences"` for explicit user-stated cross-project
preferences about working with an assistant only. For that narrower recipe set
`kinds = ["lesson"]`, `evidence_kinds = ["user_statement"]`,
`required_tags = ["collaboration-preference"]`, `allow_associations = false`,
`max_summary_bytes = 384` and `max_body_bytes = 1024`. Native
validation requires lesson/user-statement-only capture, no associations, the
`collaboration-preference` tag, and summary/body limits no greater than 384/1024
UTF-8 bytes. The assessor receives explicit instructions to exclude project-only
decisions, inferred preferences, quoted speech and private project details, and
to abstain when attribution or scope is unclear. These structural checks and
instructions do not make semantic classification infallible.

Deploy the native client and matching Codex helper bundle together before adding
this field. Older native clients reject the new field rather than silently
ignoring it. A changed intent invalidates frozen selections and queued work;
start fresh sessions instead of rewriting retained ledgers.

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
mnemed config inspect --config /absolute/memory.toml --cwd /absolute/project --offline
mneme-mcp --memory-config /absolute/memory.toml
```

Ordinary CLI commands use `--memory-config PATH` (not the legacy library/TUI
`--config` flag). `--store NAME` deliberately selects an admitted configured store,
including an inactive one, without changing automatic activation. Profile and
exclusion boundaries still apply. `--user` refuses under declarative configuration:
store names are not hard-coded global/project roles.

Selection is metadata-only, bounded and digest-qualified. With discovery enabled,
initial acquisition may contact catalog publishers; retained selection and exact
revalidation use frozen receipts without refetching. See [store discovery](discovery.md)
for recursive precedence, explicit refresh and failure handling. It reports configured
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

Opt-in [on-use cache population](../integrations/library/CACHE.md) adds
`cache.populate` to this owner-first policy. Successful canonical payload reads
record demand; configuration inspection does not. The separate local worker copies
verified nodes and portable bodies with their binding, without following file links.
CLI/TUI/MCP reads preserve snapshot provenance; automatic hooks remain canonical-only.

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

## Native project setup

Ordinary `mnemed init` detects native configuration through the shared workspace
resolver. It does not choose legacy setup because a configured owner is inactive,
unavailable or denied. Invalid configuration and known policy exclusions refuse
before creating a store. Use normalized root paths without `..` components.

Setup creates or adopts `.mneme/codex-memory.db`, preserving an existing native
database identity and its bodies. It appends a project owner declaration to
`.mneme/memory.toml` and validates the resulting native selection. An unambiguous
discovered name for that identity is reused; explicit local name/identity claims
are not overwritten. Existing legacy project selectors need explicit migration,
not an automatic profile rewrite. Global store definitions, libraries, defaults,
delivery and background policy remain unchanged. In particular, disabling core
delivery is not the same as denying ordinary reads.

First-time native adoption can choose a new local owner port. Its project overlay
does not overwrite an existing discovery entry: readers outside that project
continue to use its published endpoint. Choose the intended stable port with
`--port`, or update the owning publisher separately and then refresh discovery in
the affected workspaces. A refresh acquires published configuration; it does not
publish the project's local override.

For new stores, optional `[setup.discovery]` defaults select `mode = "auto"`,
local hub names and a client-reachable `advertise_url`. `mnemed init --interactive`
asks for registration settings for one invocation; `--register` with
`--discovery-hub`/`--advertise-url` supplies them directly, and `--no-register`
suppresses publication. This uses add-only hub registration after local readiness;
failed destinations do not undo setup, and an identical rerun is safe. See
[register new project stores](discovery.md#register-new-project-stores) for the
hub option, full configuration and device-route examples. Without this opt-in,
setup remains local. Low-level `capture init` does not register with hubs.

The project MCP entry adds managed local startup before handing the untouched
protocol stream to the **full configured native router**. If a supported shared
native registration selects the same memory configuration, setup overrides that
same server name only in the project layer. Tool filters, approvals and environment
remain inherited; secrets are not copied into project configuration. Ambiguous,
disabled or fixed-workspace registrations refuse rather than being repaired or
duplicated. Without a matching shared entry, setup uses `mneme_project`.

The launcher checks the actual process workspace, not a forced parent workspace.
Ordinary subdirectories retain native scope; a different nested project or privacy
boundary cannot start or use the parent's managed owner through this entry.
Native metadata discovery remains bounded and policy-controlled. Exact selections
are replayed offline before and after owner readiness; policy drift is not a reason
to retry through another route. Bare CLI commands remain connect-only: when the
owner is stopped they report unavailable; launching MCP can start the owner again.
This is not continuous service supervision.

No project lifecycle hooks or device actor are installed or trusted by native
init. Existing device actor configuration and trust remain unchanged; prepare that
actor separately if it is absent. `--no-recording`, `--library-config` and
`--store-registry` cannot independently override native policy. `--codex-binary`
also applies only to non-native setup; configure the native actor separately.

Before configuration commits, failure restores only unchanged installer-owned
outputs and the known overlay preimage. Concurrent edits are retained for review.
After the overlay, service and final selection have been checked, an owner-start
failure reports **incomplete** and retains that coherent configuration for a
validated same-route rerun. A newly created store whose final identity-specific
admission fails may also remain as resumable data. Setup never deletes, resets or
rolls back a database or its bodies. Keep the reported receipts and rerun after
resolving the refusal; do not delete memory data to recover setup.

## Prepare one Codex device actor

Use Python 3.11+ and a coherent published helper bundle. This command prints a
reviewable plan; it does not publish config, grant hook trust, start a service or
contact a database:

```sh
python3 /runtime/lib/registry_config.py prepare-native \
  --memory-config /absolute/memory.toml --native-cli /runtime/bin/mnemed \
  --cwd /absolute/project --state-dir /absolute/retained-device-state \
  --reader-codex codex --hooks-program /runtime/lib/hook_launcher.py \
  --hook-config-path /absolute/generated-hooks.json
```

The resulting hook adapter holds native config/executable selectors and retained
state directories. `native_cli` may be `"mnemed"` (resolved from current `PATH` for
each native request) or an absolute launcher path. Executable editions are not
store identity: replacing a tool does not invalidate the selected owner, refill
accounting, or weaken native config, receipt and database checks. Unsupported
flags, protocol mismatches and missing commands are ordinary bounded failures;
there is no whole-version admission gate. The background reader defaults to `"reader_codex": "codex"`, with
no `reader_codex_sha256`: each fresh reader process resolves `codex` from the hook
process's `PATH`. Updating that launcher does not change the adapter or refill
packet budgets; an already-running reader process keeps its existing executable.
Preparing this configuration does not require `codex` to be installed.

`--workspace-adapters` accepts an explicit JSON list of
`{root,state_dir,reader_codex}` machinery bindings. Use `"codex"` for the same dynamic
lookup, or an explicit absolute launcher path. Absolute tool paths preserve
symlink spelling and do not implicitly request a hash pin. A stable package-manager
launcher can therefore select updated tooling without changing the adapter.
Existing explicit SHA-pinned configurations remain supported and are not rewritten
implicitly. New preparation omits executable SHA fields; helper bundle source
hashes and installation receipts still protect coherent publication.

Stable POSIX hooks dynamically discover a compatible Python 3.11+ interpreter
on `PATH` at every launch, including later directories when an older interpreter
shadows a compatible one. New stable-hook plans use v2; retained v1 plans still
render their original wrappers for rollback. The stable trusted launcher itself
does not change. Explicit absolute interpreter selectors retain their lexical
path. A running worker keeps its actual process identity and interpreter;
only later launches see changes. Windows authoring preserves the established
setup-selected interpreter for the default; it is stored once, not substituted
while reconstructing receipts. Dynamic compatible-PATH interpreter discovery is
currently a POSIX contract. Explicit Windows selectors remain lexical and unpinned.
Claude and legacy shadow readers follow the same
unpinned selector contract. MCP client selection uses an explicit
`MNEME_CLIENT_BINARY` override, then current `PATH`, then a packaged sibling when
no PATH command exists; a selected command failure is not retried elsewhere.
These machinery bindings preserve existing ledgers; they do not select stores or
grant background authority. Core and asynchronous lifecycle recipes are emitted
independently and consult native configuration when invoked.

An optional project-scoped migration/package route,
`install.py prepare --memory-config PATH`, stages the
configured MCP registration and emits **no project hooks**. Supply the explicit
native binaries, project root and absent immutable prefix; the reader defaults to
launch-time `PATH` lookup. Do not also supply old registry/model/recording policy
flags. An explicitly pinned
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

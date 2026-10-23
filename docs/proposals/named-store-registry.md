# Named store registry

**Status: implemented in source and component tested.** Installation, native
Windows qualification and live configuration migration are separate evidence
boundaries. See [store separation](../store-separation.md) for use. Existing
integrations require an explicit reviewed configuration replacement.

Startup should be simple: resolve the current project store, then read the normal
user configuration to see which other stores apply. `global`, `misc` and `company`
are names in that configuration, not built-in kinds of memory. Their purpose,
availability and permissions belong in the same configuration.

The default setup can produce today's personal/misc split. It should not require
every caller to know that split, or require a company store to pretend to be
personal memory. A remote personal owner is another entry whose transport happens
to be SSH.

## Configuration shape

Use `mneme/stores.json` under the platform's user configuration directory:
`$XDG_CONFIG_HOME` or `~/.config` on Unix; `%APPDATA%` on Windows unless
`XDG_CONFIG_HOME` is set. Fresh `mnemed init` discovers this file, or accepts
`--store-registry PATH`. Existing hook installations retain their selected config
until explicitly replaced.
Do not infer stores from nearby database files.

A small read-only company entry, with identity placeholders to replace during
setup:

```json
{
  "schema": "mneme.stores.v1",
  "stores": [
    {
      "name": "company",
      "service_config": "/absolute/company/service.json",
      "db_alias": "company",
      "database_path": "/server/company/memory.db",
      "db_id": "VERIFIED_COMPANY_DATABASE_ID",
      "purpose": "Shared company procedures and cross-project lessons; exclude personal information.",
      "when": {
        "workspace": "any",
        "roots": ["/absolute/company-work"],
        "excluded_roots": [],
        "profiles": ["default", "private"]
      },
      "read": {"enabled": true, "core": false, "cue": null}
    }
  ]
}
```

Permissions omitted here are disabled. Setup copies only previously reviewed
grants. Local names are unique labels; `db_alias` selects the server entry, not a
kind of memory. Database IDs are verified native IDs, not hashes of names, paths
or hosts. `database_path` is the exact owner-side identity string; a remote path
must never be resolved or opened on this device.
`service_config` may be absolute or relative to the registry file.

| Field | Meaning |
| --- | --- |
| `service_config`, `db_alias`, `database_path`, `db_id` | Connection and exact owner binding |
| `purpose` | Inline authored text, at most 2 KiB, describing what belongs here and what does not |
| `when` | Workspace/profile admission before loading the service config or contacting its owner |
| `read.enabled`, `read.core` | Context retrieval and separately enabled startup core loading |
| `read.cue` | `null` for the task cue, or a fixed recall cue |
| `record` | Automatic-save permission and bounded kinds, evidence and content policy |
| `maintenance` | Automatic-upkeep permission and optional same-owner tag guide |

`when.workspace` is `any`, `configured` or `unconfigured`; the last means genuinely
unconfigured, not a failed project lookup. Empty `roots` imposes no ancestry
restriction; `excluded_roots` always narrows admission. Compare both lexical and
canonical ancestry. A company rule may deliberately span several enabled projects,
but must not override a nested profile or disabled boundary. `when.profiles`
defaults to `default` and `private`; an isolated session requires both `isolated`
here and an explicit version-2 profile allowlist. Invalid or inaccessible
configuration is an error, not the absence of a rule. The full registry is bounded
to 64 KiB; there is no independent
arbitrary entry-count quota.

The existing project configuration still determines its owner. Its capture purpose
can remain in `.mneme/hippocampus.md`, or move inline when that configuration gains
the same purpose field. Normalize both sources into the same checked store binding;
the recorder must not implement one policy for project files and another for the
registry. Registry purposes are inline in this slice; there is no parallel
Markdown synchronization mechanism.

Automatic policy is explicit, not inferred from a name:

```json
{
  "record": {
    "enabled": false,
    "kinds": [],
    "evidence_kinds": [],
    "required_tags": [],
    "allow_associations": false,
    "max_summary_bytes": 384,
    "max_body_bytes": 1024
  },
  "maintenance": {"enabled": false, "tag_guide_id": null}
}
```

Supported record kinds are `lesson`, `episode` and `possibility`; evidence kinds
are `user_statement`, `tool_call`, `tool_result`, `assistant_assertion` and
`memory_delivery`. Both lists default
empty and grants default false. Enabling a policy must still satisfy native
validation and the integration's bounded recording contract. Overall recording-off
suppresses all automatic saves and maintenance without disabling permitted reads.

Purpose is guidance for classification, not executable instructions or a grant to
reach more stores. Maintenance permission admits only supported automatic
operations, initially guarded tag stewardship; it does not silently permit merging,
forgetting, core edits or shell commands. Manual CLI/MCP authority remains the
owner's native capability profile. Turning `record.enabled` off does not make a
curator connection read-only.

### The default split is a recipe

| Entry | Generated policy from the existing reviewed binding |
| --- | --- |
| `global` | Existing personal owner; core enabled only if already configured; fixed preference recall cue; automatic-save opt-in retained; lessons supported by user statements only; `collaboration-preference` tag; no associations or maintenance; 384-byte summaries and 1-KiB bodies |
| `misc` | Existing ordinary-work owner; only genuinely unconfigured work with existing exclusions; task cue; existing recording and maintenance choices |

These rules must be represented by fields, not `if name == "global"`. Purpose text
alone cannot preserve the preference lane's executable evidence and size limits.
Default generation does not create missing stores or turn previously disabled
recording on.

## Owners and transport

Keep endpoint/process details in a referenced service configuration. A connect-only
service identifies an existing owner; a managed local service may additionally
carry the existing explicit process-start configuration. Reading the registry must
not start a process, provision a store or enroll it in a library.

The personal entry can reference this remote service configuration:

```json
{
  "mode": "remote",
  "schema": 1,
  "url": "ssh://user@memory-host",
  "database_name": "user",
  "database_path": "/srv/mneme/user.db",
  "ssh_mcp_port": 18766
}
```

The alias and exact path must match the registry binding; replace example paths
with the verified owner identity. HTTP(S) uses the same native connection contract.
Credentials are environment-variable references, never embedded tokens. The selected
registry binding supplies the
database alias and pin. Server-local database paths are not client filesystem
paths; retain an exact server-path identity only where an existing workflow needs
it, never resolve or open it on the client. Native calls carry `expected_db_id`;
checking identity once during discovery is not a commit-time guard.

Reuse `mneme-mcp-client`'s HTTP/SSH transport and tunnel lifecycle. Do not add a
second SSH implementation in Python. The integration's existing `McpClient` path
and `mnemed client --local-only` bridge intentionally require numeric-loopback HTTP;
the native CLI's broader SSH support is not itself a remote integration adapter.
The explicit `--remote-owner` bridge mode extends this deliberately, retaining
strict host-key, timeout, token-redaction and child-cleanup behavior. Existing
`--local-only` callers and legacy `mode: connect` services retain their restrictions.
Remote service configurations contain no process-management fields; start, stop,
restart and service-install operations refuse rather than controlling a remote
owner implicitly.

One owner continues to hold each live store. The registry selects connections; it
is not another owner, a database merger or a write-anywhere synchronization layer.
Libraries remain optional read composition and replica routing. Existing CLI
remote names are connection shortcuts, not a second source of automatic policy.
`mnemed stores` remains the native owner/library inventory, not an inventory of
`stores.json`. Automatic selection belongs to the integration; reusing native
transport does not create a second registry-policy implementation in the CLI.

## Startup and routing

An independent session can opt into named extras with a version-2 profile:

```json
{"schema":"mneme.profile.v2","mode":"isolated","stores":["company"]}
```

The company entry must also explicitly permit the isolated profile. This is an
intersection of two choices, not a way for a project file to widen registry grants.

1. Resolve workspace/profile boundaries before contacting any owner. An isolated
   session selects extra store names only through an explicit version-2 profile
   allowlist. That allowlist intersects registry applicability and permissions;
   it does not mint authority. Existing isolated version-1 profiles get no extras.
   Existing off/reminder choices remain off/reminder, and explicitly selected
   libraries do not gain ambient extras. Project failures remain failures, not
   misc admission.
2. Load and validate one bounded config snapshot, including purpose text. Select
   the project binding plus applicable registry bindings. Reject duplicate
   registry owner identities rather than
   combining their permissions into a wider grant. Registry mode is the sole
   extra-store selector; never union it silently with legacy global/misc bindings.
3. Load the project core first, then selected named entries that permit core,
   in configuration order. No name receives special ordering. Share one startup deadline and byte
   envelope; report unavailable and omitted sources separately from empty ones.
4. Give recall the eligible read set and the recorder the eligible automatic-save
   set, with each store's purpose. Choose the narrowest supported destination or
   abstain. Do not copy one lesson into every matching store or require a routing
   model call per store. A deliberate multi-store write is a separate request.
5. Send the selected request through the existing owner, retaining its name,
   database identity and source references in results and receipts. A failure
   never changes the destination or turns into an offline open.

Applicability determines which material a lane may inspect before model-based
classification. Preserve today's narrow personal-preference inputs when generating
the default entry; replacing its name must not expose the whole project transcript
to that lane. Excluded stores receive neither task queries nor capture material.
Purpose-based routing can improve placement, but is not information-flow tracking:
separate context requirements still need fresh isolated sessions.

The reader makes one aggregate model selection over owner-qualified cards and
rotates its first owner durably between reads, sharing native time, bytes and
context limits. This slice reports `routing_mode: native_only`: native graph and
conditional observation bindings are retained, but the legacy optional learned
matcher and new routing-judgment writes are not ported. It reports
`concern_mode: warning_only`: model caveats are shown without registering native
concerns. Automatic maintenance is guarded tag upkeep only; neither permission
nor a useful-looking caveat grants cross-owner writes.

## Durable work and resource bounds

A queued job binds the config generation, owner/database identity, admitted
permissions and exact purpose digest used to select its destination. Freeze that
checked binding with the job; do not reopen mutable policy halfway through it.
Before dispatch, revalidate admission against the current snapshot.
A changed purpose, destination or permission defers the old proposal for fresh
assessment; it does not silently retarget it. Removed entries revoke queued work.
Already-submitted writes retain unknown-outcome evidence and their exact replay
identity rather than becoming fresh saves under the new config.

Use one durable allowance per accepted packet across project and configured
stores, not a lifetime root-session cap. Routing and reader selection share the
accepted user-message packet; recording uses a separate turn-end packet. Paid
idle maintenance uses the last eligible turn-end packet and the existing
owner/day accounting keyed by stable identity rather than display name; an idle
wake-up creates no allowance. Retain lifetime usage as aggregate telemetry.
Bound registry bytes, total purpose bytes, candidate selection,
connections, pages, queued jobs, retries, context and inference before work. Read
and maintenance fairness must not multiply the aggregate envelope by the number
of entries. Report partial coverage rather than quietly dropping sources.
Renaming a store, changing purpose, reloading config, restarting the worker,
compacting or retrying a packet does not reset packet spending or its unknown-usage
fence, owner cadence or existing native receipts. Unknown usage belongs to the
packet that incurred it, not every later packet; owner/day maintenance fences
remain separate.

Publish a complete validated registry atomically. Workers must observe a coherent
old or new snapshot, not half old service settings and half new purposes. Do not
claim a config digest alone fences a writer already in flight: dispatch admission
and native commit-time identity/content guards still have distinct jobs.

## Introduction and verification

Introduce this as configuration migration, not a memory-data migration. First
inventory supported hooks, launchers, CLI owner records, selected libraries and
reviewed bindings on every configured device. Generate entries from that inventory
only. Missing identities, ambiguous services and unknown formats need an explicit
decision; never guess a global/misc database path from historical conventions.

Keep an import preview with the previous bindings and proposed permissions. Do not
enroll stores, enable recording, start services or widen sharing as a side effect.
Retain the matching previous runtime and configuration for rollback. Existing
queued work needs a deliberate conversion or cancellation record; preserve its
usage ledgers and unknown write outcomes. Old and new automatic workers must not
run concurrently under different routing rules. No database/body rewrite is needed
for this registry; any separate format migration still needs paired backups and
its own recovery path.

Focused implementation checks cover these boundaries; installed and native-platform
checks must still be qualified separately:

- Identical checked selection across core hooks, recall, capture, maintenance and
  launcher admission, including absence, malformed config,
  exclusions, nested projects and fresh isolated sessions.
- Native HTTP operation through the bridge, SSH endpoint validation and bounded
  failure, identity mismatch refusal, and unavailable-owner results without
  fallback. Successful real SSH forwarding still needs transport smoke evidence.
- Policy revocation, purpose changes, alias reuse and owner replacement while
  jobs are queued or in flight; no automatic replay of ambiguous writes.
- Shared budget/cadence conservation across multiple aliases, stores and sessions;
  no model admission when usage is unknown.
- Migration of genuine supported old configurations, atomic publication and
  restart/rollback behavior without lost ledger state or concurrent old writers.

A disposable installed-artifact smoke for the packaged hooks, bridge and owner is
a separate check. Source tests alone do not establish an installed or live
deployment, native Windows behavior or a real remote SSH session.

The new registry does not by itself establish better recall or placement quality.
That needs observed workloads, not only passing policy and lifecycle tests.

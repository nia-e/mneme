# Mneme for Codex

Add persistent project memory to Codex: recall earlier decisions, inspect their
sources, and save useful lessons or events for later sessions. Project setup also
enables a background reader and selective recording of completed work.

[Set up a project](#ordinary-project-setup) · [Use memory](#save-a-memory) ·
[Configure scope](#explicit-project-profile-and-library-selection) ·
[Advanced setup](#advanced-installation) · [Disable](#persistent-off-switch)

## Ordinary project setup

You need Codex with lifecycle hooks and app-server support, Python **3.11+**,
and matching `mnemed` and `mneme-mcp` binaries with persistent storage and HTTP
support. Background reader/recorder calls need
Codex authentication in `auth.json` under `CODEX_HOME` (normally `~/.codex`).
From the Mneme checkout, install the package defaults:

```sh
cargo install --locked --path crates/mnemed
cargo install --locked --path crates/mneme-mcp
```

Then, from the project you want to remember:

```sh
mnemed init
```

This creates or adopts `.mneme/codex-memory.db`, installs the project integration
under `.mneme/codex-integration`, starts its local owner, writes `.mneme/cli.json`,
and registers the verified owner in the device-local library. It does not share
memories with other devices or install a boot service.

**The default enables model-backed recall and automatic recording.** Task cues,
candidate memories and selected session messages/tool results can reach the
configured Codex model provider. Do not enable it for material that must remain on-device.
To set up recall without automatic recording:

```sh
mnemed init --no-recording
```

Reruns preserve existing off choices. They do not replace custom owners, private
or isolated configurations, or disabled hooks. If setup fails, keep the database
and installation receipts; `mnemed --json init` reports the stage and recovery state.

Useful options:

| Option | Purpose |
| --- | --- |
| `--root PATH` | Select a project other than the current directory. |
| `--mcp-binary PATH`, `--codex-binary PATH`, `--python PATH` | Select installed prerequisites explicitly. |
| `--port PORT` | Choose the local owner port for new setup. |
| `--library-config PATH` | Select and retain a device-local library config. |
| `--no-trust-hooks` | Leave generated hooks pending manual review. |

By default, init trusts only its exact generated, enabled project hooks through
Codex's native config API. Missing APIs leave trust pending rather than bypassing
review. If trust remains pending, use `/hooks` to review them.
After setup and trust review, start a **fresh Codex session**—including when init
trusted the hooks successfully. Project-local hooks also require a trusted
project config layer. See the [official OpenAI hook documentation](https://learn.chatgpt.com/docs/hooks#review-and-trust-hooks).

## Save a memory

In Codex, ask for the operation you want:

- “Recall earlier decisions about this project's storage layout.”
- “Inspect the source of that memory before relying on it.”
- “Save the verified lesson from this fix for future sessions.”
- “Record this incident as an episode, including what we observed.”

Use `db: "project"` with the project MCP tools. Personal memory, when separately
configured, uses `db: "user"`; project operations never fall back to it. Keep the
store with every returned ID.

The CLI uses the owner configured by init, so ordinary commands do not open a
second copy of the database:

```sh
mnemed query 'storage layout decisions'
mnemed get NODE_ID --body
mnemed save 'A verified lesson, with its scope and evidence'
mnemed save --kind episode 'What happened during this incident'
```

A note is a reusable conclusion; an episode is an account of an event. Neither
becomes always-loaded core automatically. Check current state before treating
remembered advice as still applicable.

<a id="source-keyed-capture"></a>

### Source-bound saves and installed compatibility

For a reproducible MCP save, include a stable source key:

```json
{
  "db": "project",
  "source": {
    "namespace": "codex",
    "key": "<stable session/turn/claim key>",
    "reference": "<actual session, source file or artifact>"
  },
  "summary": "<searchable lesson>",
  "body": "<evidence, scope and applicability>",
  "tags": ["storage"]
}
```

For CLI JSON, omit `db` and run `mnemed save --input memory.json`. The configured
owner selects the destination. The fallback bridge below also omits `db`.

The same source namespace/key and identical content replay the original ID;
changed content conflicts. Unsourced saves use an operation ID instead. Preserve
that ID and content for retries (`--operation-id` for CLI text or `operation_id`
in JSON); `source` and `operation_id` are mutually exclusive. After an ambiguous
write, inspect the outcome rather than issuing a fresh duplicate save.

Older installed integrations retain `capture` and `episode` append compatibility.
Select those only when their tool catalog lacks `save`, not as a retry after an
uncertain write. Existing storage must support the chosen runtime; see the
[offline upgrade guide](../../docs/episodic-memory.md#existing-persistent-stores).

### Relationships and learning

A note may include up to eight explicit same-store links:

```json
"links": [{"to": "<existing node ID>", "kind": "associative", "weight": 0.5}]
```

Inspect targets first. `transition` and `derived_from` are also supported.
Changed links change the source-keyed request; replay does not restore removed
edges or reset learned weights. Use `link` to connect existing notes rather than
saving them again. Recall is read-only; `walk` and `reflect` are separate deliberate
learning operations. See [CLI and MCP usage](../../docs/cli-and-mcp.md).

## Selected episodes

To save an event through MCP:

```json
{
  "db": "project",
  "kind": "episode",
  "source": {
    "namespace": "codex",
    "key": "<stable event key>",
    "reference": "<actual session or artifact>"
  },
  "summary": "The viewer showed no edges because its response omitted them",
  "body": "The store still had edges. We checked the display path before rebuilding it.",
  "thread": "viewer-edges"
}
```

The `episode` tool provides focused reads:

```json
{"db":"project","action":"search","cue":"viewer empty edges","limit":4}
```

```json
{"db":"project","action":"get","episode_id":"<returned ID>","body":true}
```

Include `edition_id` when citing an exact account. Corrections create a new
edition without replacing past references. See the [episode guide](../../docs/episodic-memory.md)
for timelines, occurrence times, history and revision requests.

## Operating the configured service

Use native MCP tools when available. The installed bridge is useful in a thread
that has not loaded the new MCP registration:

```sh
python3 .mneme/codex-integration/lib/memory.py \
  --service-config .mneme/codex-integration/config/service.json \
  recall --text 'storage layout decisions'
python3 .mneme/codex-integration/lib/memory.py \
  --service-config .mneme/codex-integration/config/service.json \
  save --input memory.json
python3 .mneme/codex-integration/lib/service.py \
  --config .mneme/codex-integration/config/service.json status
```

Use your selected Python 3.11+ interpreter. The bridge also supports `core`,
`get ID`, `episode --input FILE`, and `supersede --winner NEW_ID --loser OLD_ID`.
Add `--no-start` before the subcommand to require an already-running owner.
Inputs accept `-` for stdin where supported.

<a id="service-lease"></a>

## Hosts, routing, and local components

Each store has one shared HTTP owner. The launcher starts or reuses that owner;
Codex sessions do not each open a database. While it is running, use MCP, the
configured CLI, or the bridge—not `mnemed --db PATH` or another host on that store.

Installed helpers use their sibling `bin/mnemed`. Updating a binary elsewhere on
PATH does not update the copied integration. Use a matching native pair; builds
with different embedders are not interchangeable on an existing store.

A managed-local service config uses absolute paths:

```json
{
  "binary": "/absolute/runtime/bin/mneme-mcp",
  "database_name": "user",
  "database_path": "/absolute/stores/user.db",
  "port": 18766,
  "state_dir": "/absolute/private/global-state",
  "working_directory": "/absolute/workspace"
}
```

The database must already exist. `service.py --config FILE start|status|stop|restart`
manages that owner; `print-launchd` emits a template. Optional `token_env` names
an existing environment variable, never a token value. Global and project owners
need separate state directories and ports.

### A remote global store through a loopback forward

A connect-only service config reaches an existing owner without starting or
stopping it:

```json
{
  "mode": "connect",
  "url": "http://127.0.0.1:18767/",
  "database_name": "user",
  "database_path": "/absolute/server/stores/user.db"
}
```

`database_path` is the server's exact normalized absolute POSIX path, not a local
file. The endpoint must be a numeric-loopback HTTP root URL on port 1024–65535.
Optional `token_env` supplies authentication from the environment.

For an SSH forward:

```sh
ssh -N -T -o BatchMode=yes -o ExitOnForwardFailure=yes \
  -L 127.0.0.1:18767:127.0.0.1:18766 user@remote-host
```

Manage the remote host and tunnel separately. `launcher.py` and `memory.py` accept
this config; `service.py` supports only passive `status` for it. Moving a global
store requires a consistent database/body backup, verified identity and one
writable owner—not syncing live files. See [remote CLI setup](../../docs/remote-cli.md).

## Explicit project profile and library selection

An optional `.mneme/profile.json` selects scope before contacting owners:

```json
{"schema":"mneme.profile.v1","mode":"isolated"}
```

| Mode | Behavior |
| --- | --- |
| `default` | Normal project routing and selected personal/library context. |
| `private` | May read selected core, but is excluded from library descriptor sharing. |
| `isolated` | Only explicitly selected independent project/library memory; no personal fallback. |

Add `library_config` with an absolute library JSON path to select a library.
An explicitly selected library without `core` routing does not fall back to ambient
personal core. For isolation, start a fresh session: a fork cannot erase personal
context already loaded. Configured, private, isolated, disabled and reminder-only
work never falls through to a device misc binding.

For library MCP access, register `lib/library_launcher.py`, not the native
personal library binary directly:

```sh
python3 /absolute/prefix/lib/library_launcher.py \
  --binary /absolute/prefix/bin/mneme-mcp \
  --default-library-config /absolute/private/library.json
```

The wrapper applies project selection before starting the native stdio library
host. An isolated project needs an independent library config. The project
installer does not create a global library registration.

Local registration by `mnemed init` is not relay enrollment. For optional sharing,
see the [library and relay guide](../library/README.md). Low-level installation
can pin `--library-helper /absolute/source/integrations/library/library.py`;
existing stores require the additional `--enroll-existing` option. Enrollment
needs a selected default profile/library and live owner identity verification.

## Core first; task recall separately

Always-loaded core is optional and separately configured. The passive core hook
loads global `user` core, then the current allowlisted project's `project` core
on startup, resume, clear and compact. It does not search, start owners or save
memories. A missing owner is reported as unavailable, not empty.

Save a private config such as:

```json
{
  "schema": "mneme.codex-core.config.v1",
  "global_service": "/absolute/global/config/service.json",
  "global_database": "/absolute/stores/user.db",
  "projects": [
    {
      "root": "/absolute/project",
      "service_config": "/absolute/project/.mneme/codex-integration/config/service.json"
    }
  ]
}
```

Use `projects: []` for global-only context. Register the pinned
`core_hook.py --config /absolute/private/core.json` as a `SessionStart` command
for those four sources, and review it through Codex hook trust. The global path
must match its service; project services must select that root's
`.mneme/codex-memory.db`. Loading has a twelve-second outer watchdog and an
18 KiB output bound. Give the Codex hook at least fifteen seconds so its own
timeout does not preempt collection and cleanup. Native startup and reads share
the remaining scope allowance; they are not cut into subsecond exchange budgets.
This is separate from init's project hooks and project allowlist.

## Advanced installation

Use the low-level installer when you need a reviewed runtime prefix, a nondefault
recall mode, or an explicit owner binding. Run these from the Mneme checkout and
replace `/absolute/...` placeholders. The prefix must be absent.

```sh
python3 integrations/codex/install.py prepare \
  --project-root /absolute/project --prefix /absolute/new-prefix \
  --mnemed /absolute/bin/mnemed --mcp /absolute/bin/mneme-mcp \
  --port 18765 --output /absolute/private/install-plan.json
# Review install-plan.json before applying it.
python3 integrations/codex/install.py apply \
  --plan /absolute/private/install-plan.json
```

Unlike `mnemed init`, this defaults to **reminder recall and recording off**.
It pins/copies a runtime and edits owned project Codex entries, but does not
create storage, start an owner, or trust hooks. Initialize an absent explicit
store separately with `mnemed --db PATH capture init`; its parent must exist.
For existing current storage, `capture inspect` checks identity without inference.

The installer refuses unsafe concurrent edits. Use `recover --pending PREFIX/pending.json`
for an interrupted apply, or `uninstall --receipt PREFIX/receipt.json` to remove
unchanged owned wiring. Keep receipts and memory data when resolving failures.

For hook-only replacement against an existing managed-local owner, add
`--existing-service-config /absolute/old-prefix/config/service.json`, selecting
its exact binary with `--mcp` and existing port with `--port`. Remove unchanged old
project wiring using its receipt first; retain the owner runtime/state. The root
and database must match. Connect-only or foreign owners are not supported here.

<a id="async-reader-prototype-opt-in-project-only"></a>

### Async reader (opt-in, one explicitly selected store)

Add these options to `prepare` for background task-shaped recall:

```sh
--recall-mode async --reader-model gpt-6.1-sol \
--librarian-effort medium --reader-codex /absolute/bin/codex
```

The reader selects source-backed context from the configured store and delivers
it at eligible tool boundaries. Late results are discarded; no-tool turns may
receive no automatic context. Manual recall remains available.

`low`, `medium`, and `high` select resource presets, not relevance quotas or
billing caps. Medium admits up to 48 attempts / 250k input / 20k output tokens
per session; low halves and high doubles these thresholds. The final admitted
call can overshoot. Reader, routing and recording share accounting; missing usage
halts further model admission. There is no automatic provider retry or escalation.

### Automatic recording (separate opt-in)

Add `--recording-mode automatic` with async recall. After completed root-session
work, a background assessor may save one scoped lesson, open possibility or
episode—or nothing. It uses selected session messages and tool results, not a transcript archive.
Uncertain writes remain unresolved rather than being replayed automatically.
Recording does not automatically merge memories, forget nodes or edit core.

#### Shared tags and background upkeep

Fresh preparations with automatic recording also enable `tag_stewardship`.
The existing assessor can propose ordinary topic tags using bounded native
vocabulary context. Idle work revisits changed and older notes, applying only
content-guarded tag edits. It shares session accounting and a device-local owner
allowance; it does not run inference on reads.

Use `--no-tag-stewardship` to leave this off, or `--tag-guide-id ID` to select a
same-owner guide whose complete canonical summary is at most 2 KiB. A guide body
is background, not classifier policy. Existing configurations without the new
field remain off; recording off also disables this work. Update through the
reviewed installer rather than replacing individual live files or resetting
usage ledgers. See [hippocampus stewardship](../../docs/tag-stewardship.md) for
vocabulary browsing, unknown outcomes, limits and read-only journal inspection.

#### Per-project capture and association guidance

For an enrolled project with automatic recording, optionally add
`.mneme/hippocampus.md`:

```markdown
Favor verified storage invariants and decisions that changed implementation.
Keep speculative ideas visibly speculative. Associate lessons with retained
design notes only when the evidence supports that connection.
```

The file must be regular UTF-8, at most 2048 bytes; it and `.mneme` must not be
symlinks. It steers recording, not recall or the task agent. It is not read for
misc/workshop scope or when recording is off. Reload configuration and start a
fresh session after changes.

### Lazy advisory curation

The reader may show tensions between memories. With recording enabled, supported
findings can be retained through native `concern` operations. This is advisory:
keep both accounts unless a separate deliberate curation action is justified.
Read-only mode can show caveats but does not register findings.

### Authored touchstones

[Touchstones](../../docs/touchstones.md) preserve deliberately authored significance
and historical references. The reader can surface their existing facets; recording
cannot author touchstones or rewrite protected fields.

### Isolated shadow pilot (opt-in)

For a disposable isolated project, use `--recall-mode shadow`,
`--shadow-model gpt-5.6-sol` (or `gpt-5.6-terra`) and
`--shadow-codex /absolute/bin/codex`. A separate assessor records local observations
of bounded context delivery, without memory writes. Task material can reach the
selected provider. This is an experimental mode, not a quality guarantee.

The older `--recall-mode automatic` foreground read mode remains available;
use `install.py prepare --help` for the accepted mode options.

### Cross-project collaboration preferences (explicit opt-in)

With async recall, pass `--global-preferences /absolute/private/owner-binding.json`:

```json
{
  "service_config": "/absolute/global/config/service.json",
  "database_path": "/absolute/stores/user.db",
  "db_id": "<verified native database ID>"
}
```

This selects an existing personal owner without creating it or changing its
registration. Global recall uses a fixed preference cue, not the project prompt.
Automatic recording additionally needs `--recording-mode automatic` and may retain
explicit user-authored durable collaboration preferences—not project facts or
copied logs. Scope selection is model-based, not a guarantee against leakage.
Isolated projects and incompatible selected libraries deny this lane.

### Explicit workshop/user target

For workshop continuity in an existing `user` store, prepare a separate hook config:

```sh
python3 integrations/codex/workshop_config.py \
  --workspace-root /absolute/workshop --state-dir /absolute/private/hook-state \
  --service-config /absolute/global/config/service.json \
  --database-path /absolute/stores/user.db --db-id VERIFIED_DATABASE_ID \
  --reader-codex /absolute/bin/codex > /absolute/private/workshop-plan.json
```

This emits config and source hashes only. It does not install hooks or contact
an owner. Deployment must copy the pinned programs/config and register/review
the hooks separately. The generated policy uses `gpt-6.1-sol`, medium effort and
automatic recording. Nested project/private/isolated boundaries are excluded.
A workshop reminder-only configuration instead uses `memory_scope: "workshop"`
and `recall_mode: "reminder"`; it never provisions storage.

### Device misc default (explicit enablement)

For unconfigured, non-excluded work, prepare a device-wide binding to an existing
ordinary-work owner (`project` alias), not the personal `user` store:

```sh
python3 integrations/codex/misc_config.py \
  --state-dir /absolute/private/misc-state --service-config /absolute/misc/service.json \
  --database-path /absolute/server/misc.db --db-id VERIFIED_DATABASE_ID \
  --reader-codex /absolute/bin/codex --hooks-program /absolute/runtime/hook_launcher.py \
  --hook-config-path /absolute/private/misc-hooks.json \
  --excluded-root /absolute/excluded > /absolute/private/misc-plan.json
```

This emits config, hook definitions and hashes without installing or contacting
an owner. Use a connect-mode service config; default recording is automatic,
with `--recording-mode off` available. Register the reviewed device hooks separately.
Admission excludes enrolled/configured projects, private/isolated profiles,
malformed settings and excluded roots. Keep exclusions aligned with the
[CLI misc owner record](../../docs/remote-cli.md#default-project-and-user-owners).

### Updating an advanced runtime

Use immutable prepared destination bundles. Pause hook and reader activity before
switching configuration; retain the sessions, usage ledgers and unsettled paid work.
Existing sessions can retain their cached hook commands, so changing the selected
configuration alone does not update them. For reused sessions, back up each known
old entry point and prepare a reviewed [forwarder mapping](hook_forwarder.py) to the
qualified runtime and the same owner's configuration. Only these inventoried
entry-point files are replaced; the destination bundle remains immutable.
Mappings pin the old configuration and new script/configuration bytes; configuration
changes require regenerating the mapping. An unmatched or changed mapping refuses
instead of falling back to another owner. Misc commands must retain their
`hook_launcher.py` workspace-binding entry point.

From `integrations/codex`, generate a candidate from a reviewed JSON object with
`source_script` and `routes` (the mapping fields are documented in the generator):

```python
import json
from pathlib import Path
from hook_forwarder import render_forwarder, validate_forwarder

spec = json.loads(Path("/absolute/reviewed-map.json").read_text())
candidate = render_forwarder(spec["source_script"], spec["routes"])
validate_forwarder(candidate, spec["source_script"], spec["routes"])
with Path("/absolute/prepared-forwarder.py").open("xb") as output:
    output.write(candidate)
```

The generator does not install it. Publish only after checking the original
entry point still matches its backed-up hash, then verify its cached command
reaches the prepared runtime. Review newly selected definitions through native
Codex `hooks/list` and trust only the reviewed commands; preserve disabled and
unrelated hooks. Resume normal session activity after those checks.

Long-lived MCP connections are separate from cached hooks. After changing a
managed owner's runtime, an old relay can retain its previous service configuration
and correctly refuse the new service state. Reload MCP configuration in the
**running host**, rather than starting a separate app-server: Codex's
[`config/mcpServer/reload`](https://learn.chatgpt.com/docs/app-server) queues a
refresh for loaded threads without creating a new conversation. Verify recovery
with an identity-checked read. Do not bypass the owner check or automatically replay
a failed write; a lost acknowledgement can still mean the write committed.

The new runtime upgrades the recognized project/workshop v1 checkpoint ledger
under its existing lock. It saves the exact preimage beside the ledger and changes
only the schema: checkpoint outcomes, deferred work and session identity survive.
Reader/recording usage ledgers are not reset or replayed. Ordinary event handling
still applies its normal retention and context-reset rules afterward. Unknown,
malformed or legacy misc state requires inspection; it is not silently replaced.
Store upgrades are separate offline operations with paired database/body backups.

### Hook input omissions are not owner failures

Configuration/session-ledger failures show “Automatic memory temporarily disabled.”
at `SessionStart` (including resume/compaction), not after every prompt or tool call.
An incompatible or unreadable session ledger disables automatic memory. Preserve
it and inspect the runtime/configuration and supported upgrade path above rather
than deleting it or resetting its allowances.
This does not establish that the memory owner is offline or empty. Explicit
checkpoint failures still return an error; input omissions retain their own
diagnostic below.

“Memory event skipped” means hook input was malformed or exceeded its
input bounds. It does not mean the owner failed or the store is empty. Inspect
reader/recording outcomes before changing worker capacity.

## Persistent off switch

**Stopping the owner alone is temporary:** a later MCP initialization or bridge
call may start it again. For an init-managed project:

```sh
python3 /absolute/mneme-source/integrations/codex/install.py uninstall \
  --receipt .mneme/codex-integration/receipt.json
python3 .mneme/codex-integration/lib/service.py \
  --config .mneme/codex-integration/config/service.json stop
```

If concurrent edits block uninstall, deliberately remove only the owned project
MCP/hook entries. Global core, preferences and device misc wiring are separate;
disable their relevant entries too when intended. Verify a fresh session no
longer loads them. Leave databases, bodies, runtime and private hook state intact
unless you separately intend to delete them.

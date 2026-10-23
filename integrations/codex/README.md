# Mneme for Codex

Add persistent project memory to Codex: recall earlier decisions, inspect their
sources, and save useful lessons or events for later sessions. Project setup also
enables a background reader and selective recording of completed work.

[Set up a project](#ordinary-project-setup) · [Use memory](#save-a-memory) ·
[Session activity](#session-activity) ·
[Configure scope](#explicit-project-profile-and-library-selection) ·
[Advanced setup](#advanced-installation) · [Disable](#persistent-off-switch)

For the optional capture-only Claude Code adapter, see [Claude Code capture](../claude/README.md).
Its helpers share this historical package directory, not Codex authentication or
setup. Existing Codex configuration keeps its current behavior.

## Ordinary project setup

You need Codex with lifecycle hooks and app-server support, Python **3.11+**,
and matching `mnemed` and `mneme-mcp` binaries with persistent storage and HTTP
support. Background reader/recorder calls need
Codex authentication in `auth.json` under `CODEX_HOME` (normally `~/.codex`).
For native Windows, first read the [port status and limitations](../../docs/windows.md).
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
| `--mcp-binary PATH`, `--python PATH` | Choose lexical runtime selectors explicitly; Python accepts `python3` or an absolute interpreter path. |
| `--codex-binary PATH` | Optional absolute Codex launcher for hook trust and the reader; no automatic hash pin. |
| `--port PORT` | Choose the local owner port for new setup. |
| `--library-config PATH` | Select and retain a device-local library config. |
| `--no-trust-hooks` | Leave generated hooks pending manual review. |

The background reader defaults to launch-time `codex` lookup from the hook
process's `PATH`, rather than retaining the executable found during setup.
Setup uses the current PATH executable for hook trust if available; otherwise
trust remains pending and setup can continue. Fresh native/MCP launch defaults are
`mnemed` and `mneme-mcp` from current `PATH`; setup checks its selected runtime MCP
before creating storage. An explicit absolute selector keeps its symlink spelling.
On POSIX, hooks discover compatible Python 3.11+ on `PATH` each launch. Windows
authoring preserves its setup-selected interpreter once; receipt reconstruction
never substitutes a later interpreter. An explicit Python path remains lexical.

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

## Session activity

With an [explicit activity binding](#prepare-session-activity), ask “What did
memory do this session?” or run `mnemed activity`. The default is a chooser of
retained sessions in the admitted workspace, not an assumption that the latest
session is yours. Use `mnemed activity --session codex:FULL_HOST_SESSION_ID` for
the actual ID supplied by startup guidance; `--details` adds bounded reasons,
origins and owner-qualified node IDs. Unique prefixes work only with complete
catalog coverage.

This is prospective, best-effort metadata for the automatic Codex path:
lifecycle, startup core, background recall, context handoff and recording.
Explicit CLI/MCP reads and writes, Claude capture and other hosts are not tracked.
Core preparation, recall selection, output preparation, observed host admission,
confirmed saves, maintenance and unverified capture claims remain distinct.
No observation establishes model attention or usefulness. Missing or evicted
observations are not evidence that nothing happened; old sessions are not
reconstructed from transcripts or operational ledgers.
Host admission retains the recording job identity with unknown selector
generation: matching packet text does not identify a unique recall attempt.
Conflicting latest observations in the same second remain explicitly ambiguous.

The CLI and dedicated MCP `session_activity` tool read only local metadata. They
do not open graph stores, contact owners, run models or repair/prune files.
The existing MCP `activity` graph feed is unchanged. A natural-language question
can still run the host's ordinary prompt hooks before the tool call; this report
does not pause them or already-running workers. See the
[CLI/MCP guide](../../docs/cli-and-mcp.md#session-activity) for selectors, scope,
coverage and platform limits.

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

Installed helpers resolve `mnemed` from current `PATH` for each new child (an
explicit `MNEME_CLIENT_BINARY` overrides it). A packaged sibling is a fallback only
when no PATH command exists; failure of a selected executable is not retried on an
older one. Published helper modules stay coherent, but copied native payloads do
not determine fresh configurations' launch intent. Store/embedder identity checks
still apply; changing executable edition does not change an existing store.

Native declarative hooks require coherent helper modules and a CLI implementing
the actual bounded selection/call protocol, not a metadata version match. Initial
selection may acquire bounded discovery metadata; every later native selection
check, core collector, reader child, checkpoint and reviewed install-plan check
uses `config select --expected-selection-digest` for offline replay—even when
the original configuration did not enable discovery. An older CLI that lacks
this flag refuses; the helpers never retry with fresh selection. Update the
tooling and start a fresh session after an intentional configuration or
discovery-receipt change. Do not reset an existing session ledger to bypass its
selection fence.

A managed-local service config uses absolute state/store paths and a logical or
absolute executable selector. Each new owner spawn resolves that selector; a
running owner retains its exact recorded process witness for status/stop:

```json
{
  "binary": "mneme-mcp",
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
the existing port with `--port`. `--mcp` selects the reviewed installation payload;
new plans keep the retained owner's launch selector separately. Remove unchanged old
project wiring using its receipt first; retain the owner runtime/state. The root
and database must match. Connect-only or foreign owners are not supported here.

<a id="async-reader-prototype-opt-in-project-only"></a>

### Async reader (opt-in, one explicitly selected store)

Add these options to `prepare` for background task-shaped recall:

```sh
--recall-mode async --reader-model gpt-6.1-sol \
--librarian-effort medium
```

The reader defaults to `codex` on the hook process's `PATH`, resolved whenever a
fresh reader process starts. The generated adapter keeps `"reader_codex": "codex"`
and omits `reader_codex_sha256`; replacing the PATH launcher does not invalidate
session bindings or reset accounting. An already-running process keeps its
existing executable. Preparing this configuration does not require `codex` to be
installed. Absolute `--reader-codex` paths are also unpinned and preserve symlink
spelling. Only an explicitly retained legacy SHA field requests edition checking;
new preparation does not add one. The same rule applies to `--codex-binary` and
shadow/Claude reader selectors. Native install plans separate reviewed binary
payloads from `--mcp-command` / `--native-command` runtime selectors (logical
`mneme-mcp` / `mnemed` defaults).

The reader selects source-backed context from the configured store and delivers
it at eligible tool boundaries. Late results are discarded; no-tool turns may
receive no automatic context. A missing or failing Codex executable is a background
reader failure, not a reason to stop foreground work. Manual recall remains
available.

`low`, `medium`, and `high` select per-packet admission thresholds, not relevance
quotas or guaranteed billing caps:

| Effort | Attempts | Input tokens | Output tokens |
| --- | ---: | ---: | ---: |
| Low | 24 | 125k | 10k |
| Medium | 48 | 250k | 20k |
| High | 96 | 500k | 40k |

The final admitted call can overshoot. An accepted user message owns one packet
shared by routing and reader selection; completed-turn recording owns a separate
turn-end packet. Retries, worker restarts, configuration changes and compaction do
not refill either packet. Lifetime usage counters remain aggregate telemetry,
not admission limits. Missing usage blocks further model admission for its owning
packet, not later packets; a later eligible packet can use a fresh runtime without
clearing the earlier uncertainty. There is no automatic provider retry or escalation.

Each native read round has a **512 KiB decoded-work ceiling**, shared across its
selected owners at every effort. It counts both native transport and bridge
decoding; it is not the model's context window or a target to fill. Prompt sizes,
native deadlines (10/15/20 seconds) and model token/attempt allowances still follow
the effort preset. The shared ceiling also applies to recorder and maintenance
read batches; their per-call limits, cadence and permissions are unchanged.

The reader stops fetching when its retained context packet first fills; it does
not spend the remaining allowance hunting for smaller cards or additional owners.
A known byte-budget refusal can still yield the verified prefix to the selector,
but only after the final owner/configuration check succeeds. Unknown accounting,
identity changes and failed final checks remain fail-closed. Coverage diagnostics
distinguish omitted content from owners that were not read. This bounds collection,
not the selector's reasoning: it still gets one bounded model call afterward.

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
content-guarded tag edits. Paid idle work charges the last eligible turn-end packet
and the existing device-local owner/day allowance; idle wake-ups never mint an
allowance. It does not run inference on reads.

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
and historical references. At a natural checkpoint, the foreground agent may notice
something that matters beyond its usefulness and author a touchstone itself. It
does not need a background nomination or human countersignature for its own meaning.

For configured named stores (hook configuration v12/v13), the existing automatic
recorder may instead propose a `touchstone_nomination`: a source-bound suggestion,
not a memory write or an extra model call. Preference-only capture policies do not
admit nominations. A bounded private handoff retains the nomination for a later
prompt or tool boundary in the **same session**, subject to the current workspace,
owner and recording/context policy. Recall delivery keeps its separate exact-text
receipt; nominations do not become learning evidence.

The foreground agent can accept, reword, defer or ignore the suggestion. To accept,
inspect its evidence, GET the exact same-owner references afresh, then author an
ordinary typed touchstone SAVE. If no related memory exists yet, inspect the cited
source and deliberately preserve an appropriate scene/note in that owner if it
merits saving; GET its exact node or episode edition before authoring the touchstone.
Do not invent a reference or automatically save a scene just to provide a target.
Emitted means offered, not seen, approved or saved;
ignored nominations are not negative feedback. No quota, core promotion, automatic
revision or inference of another person's feelings follows from this handoff.
The reader can surface existing touchstone facets; it cannot author their meaning.

The same recorder and local handoff can emit a `review_nomination` with a closed
`effect` variant for core creation/membership/meaning changes, supersession,
forgetting, contradiction or merge adjudication, or an episode correction. These
are grounded suggestions, never queued native requests. Only offered same-owner
references are admitted; historical episode references remain exact editions.
The foreground freshly inspects evidence and targets, then independently authors
an ordinary operation with its existing authority and actual native guards. No
new human approval requirement or atomic stale-decision guarantee is implied.
Missing an optional offer is not an unresolved durable task or negative feedback.
Ordinary background recording, advisory findings and protected-tag-preserving
upkeep remain separate; `background.core_cleanup = false` keeps existing core
cleanup in the foreground without disabling non-core maintenance.

The effect contract is shared by both capture frontends. Installer revisions
32/33 added `review_contract.py`, 34/35 added the stable-hook helper, and current
revisions 36/37 also ship `session_activity.py`. Retained plans keep their original
inventories. Pending old touchstone envelopes remain readable
without changing their original coverage or digests. A prior writer encountering
a pending unknown review envelope refuses the outbox rather than discarding it;
do not mix runtime versions or reset pending state to bypass that refusal.

Install the matching helper bundle and start a fresh session. Older helpers neither
produce nor deliver nominations; mixing helper versions or downgrading with pending
recorder jobs does not preserve unoffered nominations. Retain state for recovery
instead of resetting recording or paid-work ledgers. Existing memory records and
typed touchstone SAVE are unchanged.

### Isolated shadow pilot (opt-in)

For a disposable isolated project, use `--recall-mode shadow`,
`--shadow-model gpt-5.6-sol` (or `gpt-5.6-terra`). The assessor defaults to
`codex` on PATH; `--shadow-codex /absolute/bin/codex` selects an unpinned launcher. A separate assessor records local observations
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
  --reader-codex codex > /absolute/private/workshop-plan.json
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
  --reader-codex codex --hooks-program /absolute/runtime/hook_launcher.py \
  --hook-config-path /absolute/private/misc-hooks.json \
  --excluded-root /absolute/excluded > /absolute/private/misc-plan.json
```

This emits config, hook definitions and hashes without installing or contacting
an owner. Use a connect-mode service config; default recording is automatic,
with `--recording-mode off` available. Register the reviewed device hooks separately.
Admission excludes enrolled/configured projects, private/isolated profiles,
malformed settings and excluded roots. Keep exclusions aligned with the
[CLI misc owner record](../../docs/remote-cli.md#default-project-and-user-owners).

### Prepare session activity

Activity is opt-in during explicit integration setup; ordinary setup without these
options is unchanged. Add the following to the low-level `install.py prepare`
command [above](#advanced-installation):

```sh
--activity-root /absolute/private/session-activity --activity-device laptop
```

Review the plan's `activity` object alongside the normal installer outputs.
`apply` creates the private diagnostic root, publishes the reviewed manifest and
adds a separate project MCP entry named `mneme_session_activity`. It does not
enable extra memory work. Existing valid workspace bindings are merged, not
replaced; malformed bindings or a different existing root are refused. If the
shared manifest changes between prepare and apply, prepare a fresh plan.
Uninstall removes unchanged owned MCP wiring but retains the shared manifest and
diagnostic records, including other workspaces. A partial apply can likewise
retain those diagnostics; it must not erase shared bindings to roll back project
wiring.

For the native device workflow below, add `--activity-root PATH` and
`--activity-mcp /absolute/bin/mneme-mcp` to `registry_config.py prepare-native`.
`--activity-device LABEL` is optional. Its `activity` object proposes the manifest
and separate MCP entry; preparation publishes neither. Review and publish those
alongside the other prepared integration artifacts, creating the diagnostic root
with owner-only permissions (`0700`) and the manifest as `0600`. Do not overwrite
a newer shared manifest with an older proposal. This does not modify the native
hook control schema or grant hook trust.

Activity uses its own small `activity.json`, not the native v13 hook control
configuration or `memory.toml`. The default is in the platform Mneme config
directory: `$XDG_CONFIG_HOME/mneme/activity.json`, otherwise
`$HOME/.config/mneme/activity.json` on macOS/Linux. Hooks use the same fixed path.
Both preparation commands accept `--activity-config PATH` to select an explicit
alternative. For an isolated prepared setup or test, explicitly set
`MNEME_ACTIVITY_CONFIG` to that same path for producers and pass matching
`--activity-config PATH` to the report. Merely preparing a nondefault path does
not redirect running hooks. No missing binding triggers owner discovery or a
global-memory fallback.

The manifest admits only reviewed canonical workspace paths and a private,
canonical absolute diagnostic root. It contains no owner endpoints or memory
policy:

```json
{
  "schema": "mneme.session-activity.config.v1",
  "root": "/absolute/private/session-activity",
  "workspaces": [{"path": "/absolute/workspace", "source": "codex"}],
  "device": null
}
```

The manifest is bounded to 64 KiB. A device label is optional; `null` means
unknown, not a machine identity inferred from the filesystem. Retained records
keep their historical workspace. Reports admit only the current bound workspace,
respect nested project/worktree boundaries, and provide neither `--global` nor
remote-device aggregation.

Use a separate `mneme-mcp --activity-config PATH --cwd WORKSPACE` stdio entry for
`session_activity`; do not add it to the graph owner's tool allowlist. This entry
can report while owners are offline. Existing MCP `activity` remains the
process-local graph feed. V1 diagnostics support macOS/Linux; other platforms
return `platform_unsupported`, without changing existing integration modes.

### Stable native device hooks (POSIX)

Trust the installed Mneme integration once, rather than a new native hook
definition for each runtime release. Opt in when preparing the native device
actor:

```sh
python3 /absolute/runtime/registry_config.py prepare-native \
  --memory-config /absolute/config/memory.toml \
  --native-cli /absolute/bin/mnemed --cwd /absolute/workspace \
  --state-dir /absolute/private/hook-state --reader-codex codex \
  --hooks-program /absolute/runtime/hook_launcher.py \
  --hook-config-path /absolute/private/device-hooks.json \
  --stable-root /absolute/private/mneme-hooks \
  > /absolute/private/device-plan.json
```

Review the proposal and save its `hook_config` at the named hook-config path.
The `stable_runtime` fields identify the inputs for staging. Preparation creates
no launcher, config or trust grant; native selection may read discovery metadata.

```sh
python3 /absolute/runtime/stable_hooks.py prepare \
  --stable-root /absolute/private/mneme-hooks \
  --runtime /absolute/private/runtime-v1/hook-entrypoint \
  --python /absolute/bin/python3 \
  --hooks-program /absolute/runtime/hook_launcher.py \
  --hook-config-path /absolute/private/device-hooks.json \
  > /absolute/private/runtime-v1-plan.json

python3 /absolute/runtime/stable_hooks.py install --plan /absolute/private/runtime-v1-plan.json
python3 /absolute/runtime/stable_hooks.py select --plan /absolute/private/runtime-v1-plan.json
```

`install` stages a new real wrapper and creates or reuses the identical stable
launcher; it does **not** activate it. `select` verifies those artifacts and
atomically switches `current`. Keep the plan and immutable runtime bundle.
Existing/mismatched wrappers are not overwritten; inspect a failed stage and
choose a fresh wrapper path.
Use one operator at a time. A `selection_uncertain` result means the selector was
replaced but its durability could not be confirmed; inspect `current` before
retrying. It does not mean the previous runtime was restored.

For the initial cutover, replace—not add to—the old device/project recipes with
the proposal's `global_hooks`, then review/trust them through Codex. Refresh
already-loaded recipes or start fresh sessions. This helper does not edit Codex
configuration, grant trust, manage services, or rewrite lifecycle/accounting state.

After cutover, an update only stages a new wrapper and selects its plan. The
native command remains `/bin/sh /absolute/private/mneme-hooks/launch`; the real
launcher executes the selected wrapper, which binds one interpreter, dispatcher
and config pathname. Nix store paths belong in that wrapper, not the native
recipe. Roll back by selecting the retained old plan. Running workers keep their
existing runtime; retain old bundles until they drain.

This deliberately trusts updated installed code. It does not bypass trust for
other hooks or silently approve new native definitions. Adding an event or changing
the recipe still needs review. Changing config paths, owner selections or paid-job
inputs can still trip the existing lifecycle fences; stable native trust does not
migrate those bindings or authorize clearing them. Windows and existing direct
hooks retain their current route. `programs.mneme.enable` still installs no hooks.

The [native fixture](../../docs/evaluation/stable-hooks-20261020/REPORT.md) records
the tested Codex version, same-thread dispatch, fresh-catalog trust and controls.

### Updating a legacy version-specific runtime

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
Ordinary event handling still applies its normal retention and context-reset rules
afterward. Unknown, malformed or legacy misc state requires inspection; it is not
silently replaced.

Reader accounting upgrades Codex v2 to v3 and Claude v1 to v2 separately. The exact
legacy state and aggregate usage are retained, not reset or replayed. Migration
requires quiescent workers and no outstanding reservation or in-flight work;
unrecognized or mismatched state is refused. Migration blocks replay of the retained
active user-message identity; it cannot reconstruct already-pruned legacy packet
identities. Packet accounting is canonical in a private per-session SQLite ledger
beside the reader JSON, with the same hashed-session basename and an
`.accounting.sqlite3` suffix. It retains the exact legacy JSON bytes and their
SHA-256; reader JSON mirrors aggregate usage for existing diagnostics. Back up
and retain both together with their matching runtime and configuration. After new
accounting writes, selecting an old runtime or changing only the JSON schema is
not a rollback. Store upgrades remain separate offline operations with paired
database/body backups.

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

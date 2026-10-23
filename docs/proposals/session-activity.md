# Session activity implementation plan

Status: implemented in the working tree; final validation is recorded in the
[validation report](../validation/session-activity-20261023/README.md). No live
rollout is included. Design baseline: `995d4141923fb6d8015cd4b5e46f558463e45fea`.

Make “what did memory do this session?” answerable without doing more memory
work. Ship a small CLI report and an equivalent local MCP tool, backed by
prospective metadata receipts. No dashboard, transcript archaeology, or new graph
service. Reporting must work when memory owners are offline.

## User experience

```sh
mnemed activity                      # recent retained sessions in this workspace
mnemed activity --session a17c9e     # one uniquely matching session prefix
mnemed activity --session a17c9e --details
mnemed --json activity --session a17c9e
```

The default view groups activity by session, with first/last observed time, device
label when configured, recognizable short ID, and a compact outcome summary.
Within a session, show core, recall, delivery and recording separately; lead with
what happened and why a lane was quiet, then offer the bounded recent detail.
Use “observed” counts, not lifetime totals. “No retained observation” is not
“nothing happened.” List coverage and missing instrumentation alongside results.

The MCP name is `session_activity`. Existing MCP `activity` remains the unrelated
process-local graph visualization feed. With the actual host session ID supplied
by integration, chat can request that exact session. Without it, return a short
chooser with timestamps and IDs; never silently substitute the most recent
session for the current one.

Session selection rules:

- Match stored full host IDs, never hashed filenames. The key is source namespace
  plus full ID; support a qualified `codex:<id>` selector.
- Shorten display IDs only as far as they remain unique in the covered scope.
  Ambiguous prefixes return candidates, not a guessed winner.
- Resolve a prefix only after complete catalog coverage. An incomplete scan
  requires a full ID even if it found just one match. Full IDs permit direct
  lookup, still checked against recorded workspace and source.
- Default scope is the current workspace. MCP fixes that scope at startup; tool
  arguments cannot broaden it or supply filesystem paths. No device-global view
  or remote-device aggregation in v1.
- Canonicalize cwd and select the nearest admitted workspace binding, respecting
  nested project/worktree boundaries. Missing or ambiguous bindings are explicit
  refusals; do not silently climb into an unrelated parent. Check historical
  record scope independently, and never automatically rebind a retained session
  to another workspace.
- Details add reasons, times, scoped node IDs and provenance, never note bodies,
  prompts, queries, transcripts, provider output or credentials.

## What the report can establish

V1 records the Codex automatic path: lifecycle, startup core, background recall,
host handoff and background recording. Explicit CLI/MCP reads and writes, Claude
capture and other hosts are labelled **not tracked**, not reported as zero.
Do not infer attribution by joining process activity to session timestamps.

| Observation | Meaning |
| --- | --- |
| Core prepared | Recorded owner outcomes and bounded card counts from the normal startup core operation |
| Recall selected | A particular selector attempt chose these scoped IDs |
| Consumed for emission | The reader removed a ready packet; final hook packing can still omit it |
| Hook output prepared | The final hook result contains context for the host; not proof the host admitted it |
| Host admission observed | The normal recording observer recognized the packet in its admitted transcript slice; its exact selector origin remains unknown |
| Save confirmed | The normal save path retained an appropriate native commit/readback receipt |
| Maintenance verified | A successful maintenance operation, not a saved memory |
| Actor-reported capture | A checkpoint claim, not independent save verification |
| Quiet disposition | A recorded skip, abstention, limit, failure or omission for this attempt |

Record quiet causes where decisions actually occur: disabled mode,
non-substantive input, queued/busy, budget or unknown accounting, worker capacity,
stale/closed context, selector abstention/failure, duplicate and packing omission.
Keep policy, selection and delivery outcomes distinct. Unknown reasons stay
unknown; absence cannot establish “nothing relevant.” No observation proves
model attention or usefulness.

Current control ledgers are unsuitable as the reporting API:

- [Main hook state](../../integrations/codex/hooks.py) lacks general raw session
  identity/workspace metadata; its access helper creates locks and prunes state.
- [Reader state](../../integrations/codex/reader_worker.py) retains only limited
  recent diagnostics. Legacy `last_delivery` lacks a precise origin/time and can
  survive reset; matching a turn does not establish a valid join.
- [Recording state](../../integrations/codex/recording_jobs.py) retires ledgers and
  receipts. A verified job may be maintenance rather than a save.
- [Startup core](../../integrations/codex/core_hook.py) deliberately has no journal.
  [MCP activity](../../crates/mneme-mcp/src/activity.rs) has no hook-session identity.

Do not read those files through operational helpers, scan host transcripts, or
backfill plausible-looking history. Older sessions simply predate coverage.

## Data and producer design

Add a private, versioned activity sidecar, separate from authoritative control
state. One bounded record per source/session contains its descriptor and recent
typed observation envelopes. Reporting never reads bodies or opens graph stores.

The descriptor records the full host ID, source namespace, observed cwd and
canonical workspace binding, configured device label or `unknown`, and first/last
observed UTC time. Lifecycle/end evidence stays in the event lane. “First observed” is not
“session started.” Store captured mode and selection metadata with their time;
do not replace historical scope with today's configuration. Filesystem `st_dev`
is not a machine identity.

Each observation has an occurrence time, kind/outcome/reason and observed origin:
session, turn, generation/context generation when known, and producer attempt
identity. Selection, consumption and output retain their directly observed
binding. Host admission retains the recording job with null selector generations
and `selector_origin_unavailable`: identical packets from different attempts
cannot be distinguished through absent best-effort receipts. Do not reconstruct
that link from the sidecar, turn alone, timestamps or file mtime. Save projections retain
operation kind, historical owner identity, scoped node IDs and receipt evidence,
not arbitrary serialized receipts. Node-count overflow is an explicit omission.
Stable keys deduplicate retained retries where the host provides enough identity;
events without a native stable ID cannot establish exact retry identity. Append
sequence means recorded order, not operation order. Conflicting latest outcomes
within the same whole second are explicitly ambiguous, not resolved by arrival.

Use an allowlist schema shared through Rust/Python fixtures. Rust owns the public
typed report; Python emits the versioned local record. Unknown versions and
malformed records become coverage statuses, never an automatic migration.

Instrumentation follows real outcomes. It snapshots a small safe projection under
the existing producer's synchronization, then publishes outside control locks.
A bounded, metadata-only queue defers publication until functional selection
budgets have ended. A separate root-wide nonblocking activity-only writer lock
serializes bounded
read/modify/replace updates and quota changes; readers never acquire it. Failure
to log must not change the real operation, create paid retries, authorize a write,
or report recall failure.
All history is explicitly best effort; retain known omission/eviction counters
and covered time ranges without claiming invisible logging failures were counted.

Record lifecycle and disabled-mode outcomes before ordinary background-off early
returns. Keep the passive core reader passive: return a pure body-free projection
of its typed results after final revalidation, internally alongside unchanged hook
output, then log at the outer dispatcher. Do not parse rendered core text or
reread owners. Supply the validated host session ID in the normal startup
guidance; a missed marker still means a chooser.

Resource envelope, checked with synthetic fixtures: 64 KiB per session record,
64 MiB retained session payload and
30-day, admission-time best-effort retention, not a deletion deadline. Quota
metadata and the single atomic-write scratch are separately bounded to 256 KiB.
Event capacity derives from encoded bytes, not a quota on memories.
Writer-only retention removes oldest recognized activity records as necessary;
never touch control ledgers, unknown files or symlinks. Bound writer work and lock
time; reserve a record's full allowance at bounded session admission so routine
appends do not scan the root. Reservation recovery and pruning are producer work,
never reporting work. Skip logging if capacity cannot safely be established.
Eviction remains a coverage limitation, not evidence that a session never existed.

## One local reader and two thin adapters

Put the typed request/result and report assembly in
`mneme-app::session_activity`, with a concrete `local` filesystem adapter. No new
crate or generic backend trait for one source. CLI and MCP call exactly this
workflow; neither reconstructs evidence independently.

A setup-written `activity.json` under the existing Mneme platform config directory
binds one activity root and the admitted local workspace/source metadata. It has
no owner endpoints or graph policy. Keep it outside the strict native hook v13
configuration; do not casually change that schema to add diagnostics.
Explicit integration setup prepares the manifest and separate MCP entry. Hooks
read that same fixed platform manifest; permit an explicitly injected path only
for prepared setups/tests. Missing or malformed binding disables diagnostics
only. For reports, missing binding means `not_configured`, not a home-directory
search, discovery refresh or fallback to the private global store.
The installer requires explicit `--activity-root` opt-in; existing setup without
it is unchanged. Unrelated missing workspace bindings remain historical metadata
and do not disable reports for the current admitted workspace.

The filesystem reader uses checked read-only regular-file opens, rejects symlink
escapes, and bounds bytes before decoding. Initial report limits: 64 KiB manifest,
64 KiB per record, 8 MiB aggregate reads, 4,096 inspected directory entries and
48 KiB serialized output. Also bound JSON depth, decoded fields and work between
cancellation checks. These are reporting-resource limits, not retention guarantees.
Prioritize candidates by file mtime as a search hint, then sort verified results
by recorded observation time; mtime is never event evidence. Return truncated
coverage and “recent among scanned sessions” if enumeration is incomplete, with
full usable selectors instead of unresolvable short prefixes. A direct full-ID
lookup does not require scanning the catalog.

Expose typed statuses for missing, unreadable, malformed, unsupported, oversize,
scope mismatch and partial coverage. Atomic replacement gives a coherent individual
record, not a transaction across sessions or current owner state. No lock creation,
cleanup, repair, configuration writes, subprocess, model call or network access
on the reporting path.

CLI admission returns before normal owner configuration/discovery. Reject graph,
remote, database and profile selectors rather than silently ignoring them.
Allow an explicit CLI activity-manifest override for operators and fixtures;
the MCP equivalent is startup-only.

Use a dedicated, stdio-only activity MCP startup mode with a closed catalog
containing only `session_activity`. Ordinary configured MCP startup can perform
discovery and contact owners, so adding a local handler there is insufficient.
Do not add this tool to graph request enums or remote owner routing.
The server pins its admitted manifest, canonical workspace and root handle at
startup; editing the manifest cannot broaden an already-running instance.
The v1 filesystem adapter is Unix-specific; other platforms fail closed for this
report without changing existing memory modes.

| Surface | V1 behavior |
| --- | --- |
| Dedicated activity CLI/MCP | Local workspace report only |
| Direct owner MCP, every capability profile | Tool hidden and raw dispatch denied |
| Configured/router/library/replica MCP | Tool hidden and raw dispatch denied |
| Activity mode with HTTP or owner options | Refuse startup explicitly |
| Existing MCP `activity` | Unchanged |

The command/tool itself is read-only. A natural-language question still passes
through the host's ordinary prompt hooks before the agent chooses a tool, and
already-running work may finish concurrently. Do not promise conversation-wide
quiescence. Current admitted PostToolUse events discard tool payloads, so v1 must
not pretend it can reliably attribute or suppress particular explicit operations.
Do not widen transcript access or classify shell commands using text heuristics
to make a stronger promise.

## Implementation sequence and ownership

1. **Contract and reader.** Add `crates/mneme-app/src/session_activity/{mod,local}.rs`
   and export it from `lib.rs`. Freeze synthetic descriptor/event/report fixtures;
   implement selection, projection, bounds and coverage without producer coupling.
2. **Receipts.** Add `integrations/codex/session_activity.py`; instrument
   `hook_launcher.py`, `hooks.py`, `reader_worker.py` and `recording_jobs.py` at the
   real decision/outcome boundaries, plus the pure core projection in
   `core_hook.py`. Add shared fixtures and
   `integrations/codex/test_session_activity.py`. Carry origin bindings through
   the existing observer path only as needed; never run it from a report.
3. **CLI.** Add `crates/mnemed/src/cli_activity.rs`, wire the early route in
   `main.rs`, update `cli_help.rs`, and add `tests/cli_activity.rs`. Ship readable
   output and the same typed JSON result before integration setup changes.
4. **MCP.** Add `crates/mneme-mcp/src/session_activity_server.rs`; wire the exclusive
   startup/coordinator mode in `startup.rs` and `main.rs`. Test HTTP refusal in
   `http.rs` and catalog/dispatch denial on all sibling modes. Reuse existing
   response framing beneath the smaller activity response limit.
5. **Packaging and guidance.** Update Codex installer/preparation and embedded
   helper inventory (`integrations/codex/install.py`, `registry_config.py`,
   `crates/mnemed/src/cli_init.rs`), their fixture tests, and affected package
   manifests. Update `docs/cli-and-mcp.md`, `docs/README.md` and the Codex guide.
   Check Nix/helper inventory derivation rather than assuming an old file count.
6. **Isolated acceptance.** Exercise prepared setup, packaged CLI and MCP against
   disposable activity fixtures. Deploy to actual devices/configurations only
   under a separately explicit rollout scope.

Reader/contract and producer implementation can proceed in parallel after the
fixture contract is agreed. Frontends follow the shared contract; packaging comes
last. No changes to ranking, automatic memory policy or graph storage are needed.

## Acceptance tests and gates

- **No-contact/no-write:** deny or tripwire owner/discovery, inference, subprocess
  and network entrypoints; compare fixture filesystem inventories and hashes
  before/after every report and refusal. Check no lock/temp files are created.
- **Identity:** same ID in different namespaces, prefix collisions, incomplete
  scans, direct full-ID lookup, workspace mismatch, subdirectories, nested
  projects/worktrees, unknown device, resumed session and absent current-session
  marker. Never select “latest” implicitly.
- **Evidence:** disabled lanes, abstention, budgets, stale/reset contexts,
  selection without output, consumption followed by packing failure, output
  without admission, save versus maintenance,
  unverified checkpoint, owner-qualified duplicate IDs and deliberately missing
  observations. No body/query/provider-text leakage, including errors/details.
- **Robustness:** concurrent atomic replacement, contention/logging failure,
  corrupt/unknown/oversize/symlink files, exhausted read/output/retention budgets,
  cancellation, quota reservation recovery and writer eviction. Readers do not
  repair; writers do not alter functional outcomes when diagnostics fail.
- **Surface parity:** human/JSON/MCP semantics, complete argument admission,
  capability catalogs plus raw dispatch, stdio framing/shutdown, explicit HTTP
  refusal and owner-independent startup with unavailable owners.
- **Shipping:** relevant Rust feature combinations, Python integration tests,
  packaged helper inventory, isolated install identity and finite CLI/MCP smoke.
  No live stores or paid providers required for these acceptance checks.

Declared change-gate classes are `public-interface`, `capability` and
`transport-package`. The [validation report](../validation/session-activity-20261023/README.md)
records the complete actual file inventory, selected gates, commands and evidence
levels. No graph-store format or control-ledger format is changed; diagnostic
provenance stays outside those ledgers. The obligations are:

- `API-01`–`API-05`: typed admission, catalog agreement, representation parity,
  explicit compatibility/coverage behavior and documented happy path.
- `CAP-01`–`CAP-05`: exhaustive visibility/dispatch matrix, early denial, transport
  agreement, honest receipt semantics and generation/session fencing. The report
  must not mint authority; historical receipts survive without granting it.
- `DIST-01`–`DIST-05`: deliberate stdio/HTTP distinction, pre-work bounds, relevant
  feature builds, artifact identity and finite isolated smoke/cleanup.

The selected evidence floor is `installed_artifact`; source/component tests alone
do not satisfy it. The isolated package smoke exercises copied binaries and
helpers without live activation. Actual native-host dispatch, live deployment and
recall quality remain separate claims.

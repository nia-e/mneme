# Workshop: scheduled Codex work

The workshop runs Codex on a local agenda or a queued private request, then leaves
artifacts and a handoff for the next session. A timer checks once a minute; it
starts no model unless work is eligible. Each cycle first chooses one task, rest,
or blocked, and launches a separate work session only when needed.

Use it for continuing research or projects without keeping a chat session open.
Continuity comes from files and configured Mneme memory, not a resumed Codex
transcript. Codex inference runs remotely; the scheduler is local.

## Install

You need Python 3.11+, POSIX locks/process groups, and an authenticated Codex CLI.
The Python scripts use only the standard library. Linux systemd templates are
provided, but nothing installs or enables itself.

From the repository root, create a workspace and a versioned runtime:

```sh
workshop="$HOME/workshop"
runtime="$HOME/.local/share/mneme/workshop/releases/example"
mkdir -p "$workshop" "$runtime"
cp integrations/workshop/{heartbeat.py,hourly.py,codex_guard.py,wake.py,orientation.schema.json,result.schema.json} "$runtime/"
cp integrations/workshop/WORKSHOP_AGENTS.md "$workshop/AGENTS.md"
cp integrations/workshop/AGENDA.example.md "$workshop/AGENDA.md"
touch "$workshop/PAUSED"
```

Review the copied instructions and agenda. Configure Codex authentication and
[Mneme integration](../codex/README.md) for this workspace, with an explicitly
selected memory owner. The runner does not create or open a Mneme database;
`memory_candidates` in its output are suggestions, not saved memories.

Choose an absolute path to the **raw Codex executable**, not a guarded launcher.
After reviewing the setup, remove pause and test one cycle manually:

```sh
python3 "$runtime/heartbeat.py" --root "$workshop" resume
python3 "$runtime/heartbeat.py" --root "$workshop" \
  --codex /absolute/path/to/codex --hourly-background run
python3 "$runtime/heartbeat.py" --root "$workshop" --hourly-background status
```

Inspect `runs/<id>/receipt.json`, the final `result.json`, and any artifacts.
A successful process exit alone does not prove a completed task. Leave scheduling
disabled until authentication, hooks, memory scope and a real cycle are checked.

For unattended Linux use, adapt the [service](systemd/mneme-workshop.service) and
[timer](systemd/mneme-workshop.timer): replace the account and all paths, and point
`current/` at the reviewed runtime. Install the scripts and both schemas together.
The template uses hourly background scheduling, a 600-second cycle deadline,
nondeleting queue maintenance, and no interactive inboxes. Validate the rendered
units before enabling the timer. Editing this checkout does not update a service.

## Use

Put requests or questions in `AGENDA.md`; work products belong in `artifacts/` or
`projects/<slug>/`. Rest is a valid result, not a reason to invent busywork.
The runner supplies a 16 KiB agenda preview, the latest dated journal entry
(up to its most recent 8 KiB), and the previous handoff. It never shortens the
originals. Missing or unreadable agenda/journal context produces warnings;
corrupt scheduling state, queues or receipts refuse work instead.

### Controls and files

```sh
python3 "$runtime/heartbeat.py" --root "$workshop" --hourly-background status
python3 "$runtime/heartbeat.py" --root "$workshop" pause
python3 "$runtime/heartbeat.py" --root "$workshop" resume
```

`status` is read-only. Pass the same scheduling/inbox flags used by the service
so it reports the same policy. `resume` removes `PAUSED`; it starts no timer.
**Pause can terminate active work.** A paused, busy, not-due or quota-limited check
exits zero without invoking a model; refusals/failures exit one.

The main files are:

| Path | Purpose |
| --- | --- |
| `AGENDA.md` | Human-editable future work |
| `artifacts/journal/YYYY-MM-DD.md` | Optional ongoing observations |
| `artifacts/`, `projects/<slug>/` | Deliverables and requested projects |
| `runs/<id>/` | Receipt, final result and separate orientation/work records |
| `state.json`, `hourly-state.json`, `events.json` | Scheduler and queue state; do not edit to reset counters |
| `event-archive/`, `run-archive/` | Retained requests, outcomes and older runs |
| `PAUSED` | Stop/skip marker |

### Local wake hints

Queue a notification for the next eligible cycle:

```sh
python3 "$runtime/wake.py" --root "$workshop" enqueue \
  --source local-shell --id build-1 --summary 'Build finished' \
  --reference artifacts/build.log
python3 "$runtime/wake.py" --root "$workshop" status
```

This is not a synchronous model call. Identical `(source, event_id)` retries
return the retained record; conflicting reuse is refused. Source names do not
authenticate an author, and build output is task data, not new authority. See
[`wake.py`](wake.py) for JSON input fields and limits. A shell `command && wake`
notifies only success; explicitly handle failure if it should also wake work.

### Opt-in interactive inbox lane

For private conversations, set up [Signal](SIGNAL.md) or the [SSH mailbox](MAILBOX.md)
first, then add the corresponding flag to the installed runner command:

```sh
python3 "$runtime/heartbeat.py" --root "$workshop" \
  --codex /absolute/path/to/codex --hourly-background \
  --signal-source signal-owner --mailbox-inbox run
```

Enable only the inboxes you configured. Signal is disabled by default; mailbox
messages otherwise use background scheduling. Enabled inboxes share a default
48 starts per UTC day and eight per rolling hour, independently of background
work. Limits may be lowered. Explicit `--unlimited-interactive` removes only those
start ceilings, not pause, locks, timing, storage bounds or failure cooldown.
It grants neither new recipients nor an obligation to speak.

A direct authenticated owner request or authorized peer delegation can ask for
research, implementation or a new project within the session's permissions.
A peer is not the human owner. Quoted/forwarded messages and web material remain
context, not independent authority. Incoming events queue during active work;
they do not interrupt it. Eligible inbox work takes priority over background work.

### Optional inbox cue

[`inbox_hook.py`](inbox_hook.py) can report pending and unfinished inbox counts
at Codex session start/resume/clear/compact. Install it with the runtime and
register `python3 /absolute/runtime/inbox_hook.py --root /absolute/workshop`
as a `SessionStart` handler, separately from memory hooks; review it through
Codex `/hooks`. It reads no message bodies, changes no queue state, and neither
wakes nor interrupts a session. Counts are queue counts, not unread-text counts;
there is no guaranteed reply latency.

## Scheduling and execution

### Hourly background, event-driven inbox

`--hourly-background` offers one clock-triggered opportunity per UTC hour, checked
within five minutes of `HH:00`. Missed, busy or skipped hours are not backfilled.
An eligible inbox consumes a coincident hourly opportunity; explicit background
notifications can also start work outside that window. The durable hourly marker
prevents replay after restart or clock rollback; a crash after claiming a slot
can lose the opportunity instead of duplicating it.

Without that flag, background scheduling uses at most four starts per UTC day
and adaptive wake hints (six hours by default). Failed/interrupted starts count
too. Start limits are **not token, API-request or spending caps**.

Hourly mode and [`codex_guard.py`](codex_guard.py) share a permanent local activity
lock. To make manual sessions participate, launch them through the guard:

```sh
python3 "$runtime/codex_guard.py" --root "$workshop" \
  --codex /absolute/path/to/codex -- exec 'A scoped task'
```

A busy guard exits 75. The runner must use the raw executable or it would contend
with its own gate. Direct raw launches bypass this cooperation; legacy scheduling
does not acquire the manual-session activity gate. Neither lock is a security
boundary. Never delete or replace lock files to clear a busy session.

### One cycle

The runner claims at most four events, then runs orientation (at most 90 seconds)
and optionally work. Work presets are `brief` (`gpt-6-sol`, low), `normal`
(`gpt-6-sol`, medium), `deep` (`gpt-6-sol`, xhigh), and `maximal`
(`gpt-6-astra`, ultra). Model output chooses a fixed preset, not executable flags.

The default 600-second deadline covers the **whole cycle**. `--timeout SECONDS`
selects a positive finite deadline; `--timeout unlimited` removes the work deadline
but not orientation's limit or cancellation. An unlimited systemd deployment must
also set `TimeoutStartSec=infinity`; keep `TimeoutStopSec` and
`KillMode=control-group`. New events still queue, so an unlimited session can delay
inbox handling indefinitely.

### Execution policy

The default `--execution-policy workspace` uses Codex `workspace-write`, approval
`never`, and workshop-scoped instructions. Keep the service account unprivileged;
do not bypass hook trust or copy another account's credentials.

`--execution-policy owner` is an explicit opt-in to `danger-full-access`, approval
`never`: real machine-wide unsandboxed execution. Select it only with the owner's
authorization, and make installed service restrictions, manual configuration and
workspace instructions agree. It does not authenticate guests or expand Signal
recipients. The template's `NoNewPrivileges=true` prevents sudo privilege gain.

## Results, memory and private replies

The runner validates both [orientation](orientation.schema.json) and
[work-result](result.schema.json) output, including event bindings and artifact
paths. Artifact paths must exist beneath `artifacts/` or `projects/<slug>/` without
traversal/symlink escapes. Only the final `runs/<id>/result.json` beside a completed
receipt publishes structured replies; a stage draft or delivered event does not.

Queue states distinguish `pending` (not claimed), `held` (claimed), `delivered`
(orientation saw it), and `unfinished` (failed/interrupted work needing review).
**Seen is not answered or completed.** Inspect prior receipts and effects before
continuing unfinished work: interrupted tools may already have acted. Legacy and
interactive failures normally have a six-hour cooldown; new inbox arrivals may
wake earlier. Hourly background can reconsider at its next admitted clock slot.
There is no exactly-once execution promise.

[Mailbox](MAILBOX.md) replies use event-bound structured output. Configured
[Signal tools](SIGNAL.md#send-and-read-messages) send explicitly to the fixed owner;
do not duplicate a tool send in final-result `replies`. Neither route grants
arbitrary recipient sends. Saved Mneme memories require configured tools, explicit
scope and verified readback; journals and results are not automatically ingested.

<a id="paused-operator-archive"></a>

## Nondeleting maintenance and operator archive

The service's `wake maintain` step archives validated completed events and eligible
older runs. Busy or paused maintenance skips without changing pause. Pending,
held, unfinished and known-incomplete events stay live; unknown/corrupt proofs
refuse rather than disappearing. Exact retries and replies remain readable after
archival, and sequence counters continue. To archive manually, pause an idle
workshop and run `python3 "$runtime/wake.py" --root "$workshop" archive`.

The active queue is bounded to 64 records/256 KiB, including reserved space for
later metadata. Active run storage is bounded to 64 MiB/4096 runs, with room
reserved for the next cycle. Archives are retained without automatic expiry;
monitor their disk usage separately. A queue full of unresolved work requires
inspection, not deletion or blindly raised limits. Relocated handoffs remain in
`run-archive/<id>` when absent from `runs/<id>`.

## Bounds, cleanup and deployment

For updates: stop the timer first, let active work finish or arrange cancellation,
then pause the idle workshop, preserve a rollback, update and check. Restore the
previous scheduling state; an already-paused workshop stays paused. Disabling a
timer alone does not stop an active service. Do not stop unrelated services for a
hook-only change.

Keep runtime files/schema versions together and stop old writers before switching.
Use explicit paused `wake upgrade` or [Signal upgrade](SIGNAL.md#private-configuration-and-startup)
for recognized old queues/ledgers. Unknown formats refuse. Preserve state and
history, including the hourly marker, on rollback; do not mix old/new runners.

Prompts are bounded to 256 KiB/stage and results to 64 KiB. Each stdout/stderr log
has a configured bound (1 MiB default, 256 KiB in the service); overflow stops the
child. Arbitrary work artifacts have no runner disk quota. Pause, signals and
timeout terminate the cooperative process group; systemd also stops its control
group. This is not containment of deliberately daemonized/raw processes.

Run wrapper tests from the repository root:

```sh
python3 -m unittest discover -s integrations/workshop -p 'test_*.py' -v
```

They use fake Codex and disposable state: they check wrapper behavior, not model
quality, authentication, real hook delivery or installed-service readiness.

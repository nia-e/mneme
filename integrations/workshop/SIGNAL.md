# Private Signal inbox

The Signal bridge lets one configured owner send direct text requests to a
[workshop](README.md), and lets the agent reply to that same owner. It runs
`signal-cli`, queues incoming messages and records send outcomes. It does not
start Codex itself or expose a public network listener.

Use it for private conversation or workshop tasks, not group chat or arbitrary
recipient sends. A direct authenticated owner request guides work within the
session's permissions; forwarded/quoted third-party material does not grant
permission. The bridge does not register accounts or discover recipients.

## Private configuration and startup

First install the [workshop runtime](README.md#install), authenticate a Signal
account, and pin an executable `signal-cli` artifact and its account data
directory. Install `signal_bridge.py`, `signal_store.py`, `signal_jsonrpc.py` and
`signal_tools.py` alongside the workshop scripts. Keep the workshop paused while
commissioning; receiving can continue, but pause holds wake publication, sends
and profile changes.

Create a private JSON configuration outside the repository. Replace every example
path and both ACI UUIDs below with your own values; ACI UUIDs are canonical lowercase
account identifiers, **not phone numbers or display names**:

```json
{
  "schema": "mneme.workshop.signal-bridge.v1",
  "workshop_root": "/home/user/workshop",
  "state_dir": "/home/user/.local/share/mneme/signal/state",
  "store_path": "/home/user/.local/share/mneme/signal/state/transport.sqlite",
  "signal_cli": "/absolute/path/to/pinned/signal-cli",
  "signal_data_dir": "/home/user/.local/share/signal-cli",
  "signal_rpc_config": "/home/user/.local/share/mneme/signal/config/rpc.json",
  "account": "00000000-0000-0000-0000-000000000001",
  "owner_id": "00000000-0000-0000-0000-000000000002"
}
```

The bridge config must be a private regular file owned by the service user
(mode `0600` or stricter). Use absolute paths and canonical existing workshop,
account-data and image directories. `store_path` must be directly inside
`state_dir`, outside the model-writable workshop. Do not commit identities,
credentials or conversation text.

The separate private `rpc.json` must contain exactly:

```json
{"verbose":0,"logFile":"/dev/null","scrubLog":true,"account":null,"dbus":false,"dbusSystem":false}
```

For agent tools, add an `agent_tools` object to the bridge config with exactly
`asset_roots` (one to eight existing canonical image directories) and
`portrait_path` (an existing canonical PNG/JPEG inside one of them). This enables
a private `state_dir/signal-tools.sock`. Without it, no agent tools socket exists.
The stdio MCP adapter knows only that socket, not Signal credentials. Configure
your MCP client to launch:

```sh
python3 /absolute/runtime/signal_tools.py --socket /absolute/state/signal-tools.sock
```

Initialize a **new** state directory, inspect it, then start the bridge:

```sh
python3 /absolute/runtime/signal_bridge.py init --config /private/path/bridge.json
python3 /absolute/runtime/signal_bridge.py status --config /private/path/bridge.json
python3 /absolute/runtime/signal_bridge.py run --config /private/path/bridge.json
```

`init` refuses existing state rather than resetting history. `status` is read-only;
`run` holds a single-process lock. For supervised Linux use, adapt the
[Signal service](systemd/mneme-signal.service) to your installed runtime/config.
Installing or enabling it is a separate operator action. Add
`--signal-source signal-owner` to the workshop runner before resuming its timer;
see [inbox scheduling](README.md#opt-in-interactive-inbox-lane) for quotas and timing.

For existing state, stop the bridge and old writers, pause the idle workshop,
back up the Signal ledger and workshop state, and run:

```sh
python3 /absolute/runtime/signal_bridge.py upgrade --config /private/path/bridge.json
```

This explicitly upgrades recognized v1/v2 ledgers to v3 and the queue to v4.
`run` and `status` do not migrate them. These files are not one transaction;
an interrupted upgrade can be retried while old writers remain stopped.
Unknown formats refuse. See [`Config`](signal_bridge.py) and
[`validate_config`](signal_jsonrpc.py) for the admission checks.

## Send and read messages

With `agent_tools` configured, an inbound burst wakes an ordinary workshop
session. The agent reads retained conversation with `read_history`, then calls
`send_message` explicitly. Final `result.json` replies do **not** send Signal
messages in this mode; do not duplicate a tool send there.

- `read_history({"limit":12})` returns recent incoming and accepted outgoing text.
  The maximum is 20 entries/4096 bytes, with no pagination. Omitted or oversized
  incoming messages remain unseen. Reading marks only complete included incoming
  messages **seen**, not answered, and can consume fully covered pending wakes.
  It does not complete held/delivered/unfinished work or delete chat history.
  Reads work while paused and make no Signal RPC. A failed connection can lose
  the response after seen state commits; repeating the read is safe.
- `send_message({"request_id":"reply-001","text":"Your reply","reply_to":"<burst-event-id>"})`
  sends at most 4000 characters to the configured owner. Use the claimed **burst
  event ID**, not an incoming message ID, as `reply_to`. Keep the same key, text
  and binding on retries. Conflicting key reuse refuses. A newer owner message
  supersedes a stale bound reply before sending. Omit `reply_to` only for a useful
  proactive message; that send has no conversation-generation binding.
- `set_own_signal_profile` can set `given_name`, choose an admitted `avatar_path`,
  or `restore_portrait: true`. Images are bounded and privately staged before
  transport. Pause refuses mutations. Profile changes have no independent
  journal: a timeout after writing the request is **unknown**, not safe to retry
  automatically or report as success.

The bridge journals message intent before transport and rechecks pause, received
messages and conversation generation immediately before a send. An exact-key
retry returns its retained terminal outcome; a still-pending request can be
retried with that key, but there is no automatic direct-outbox drain.

**`accepted` means signal-cli returned an exact-owner SUCCESS and timestamp,
not proof of delivery or reading.** A known failure is distinct from `unknown`.
The bridge never replays unknown or interrupted sending attempts. Do not use a
new request ID to retry an accepted or unknown send: that risks duplicate DMs.
History's uncertain-send list is not evidence of delivery.

Without `agent_tools`, the supported final-result reply route requires a validated
completed workshop receipt and an exact event-bound reply. It retains the same
pause, generation and unknown-send rules. Neither route exposes raw messaging
CLI authority, recipient changes or arbitrary Signal RPC.

## What is durable, and what is not

Only the configured account's direct owner messages are admitted; groups and
exception envelopes are rejected. Messages are deduplicated and grouped into
bursts after 10 seconds quiet or 45 seconds maximum. Bursts are persisted before
queue publication; queue busy/full keeps them pending. Capacity refuses visibly
instead of silently dropping unanswered text.

Raw notifications first enter a bounded private spool. Complete writes survive
crashes; incomplete temporary files are retained and cause refusal. Stop the
bridge and inspect these files instead of deleting possibly acknowledged input.
Signal-cli acknowledges upstream **before** the bridge can fsync: a crash in that
gap can lose input. This is not lossless or exactly-once messaging.

[Workshop maintenance](README.md#nondeleting-maintenance-and-operator-archive)
archives completed events, not the Signal ledger or chat history. Pending and
unfinished work and uncertain sends are not discarded to make room. Seen receipts
commit before queue cleanup; a busy queue/restart may temporarily leave a redundant
wake, reconciled on a later bridge tick.

Attachments are ignored by the bridge. Upstream ignore-attachment flags do not
exclude all long-text/contact/group-sync downloads. There are no automatic read
receipts or HTTP server.

The adapter's protocol baseline is signal-cli `0.14.9-SNAPSHOT`, upstream commit
`29dcac23cf239a166b93675430bcbb7c7b4c676e`, not a claim about your installed binary.
Before commissioning, verify its version/hash and protocol with disposable
fixtures. Local tests do not certify arbitrary upstream versions or live delivery;
keep account-bound installation evidence in private operator receipts.

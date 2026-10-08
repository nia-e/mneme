# Device-local library and optional relay

The library lists known project memories and routes reads to their existing owners
or read-only replicas. The optional relay sends snapshots between trusted devices;
it is not a shared writable database.

## Local setup

[Codex project setup](../codex/README.md#ordinary-project-setup) with `mnemed init`
registers the verified project owner in the device-local library. This creates no
peer configuration, publication or replica. `status` reports these entries as
`delivery: "local"`.

To inspect the selected library, run from the Mneme checkout:

```sh
python3 integrations/library/library.py \
  --config /absolute/private/library.json status
```

The helper needs Python 3.9+ on a Unix-like system and uses only the standard
library. Library reads use native Mneme routing; see
[project library selection](../codex/README.md#explicit-project-profile-and-library-selection)
and the [library guide](../../docs/memory-libraries.md).

## Configure optional sharing

Sharing requires an explicit runtime config and a publisher sidecar. Choose stable
library/device IDs and private directories. A minimal runtime config is:

```json
{
  "schema": "mneme.library.config.v1",
  "library_id": "my-library",
  "device_id": "local-device",
  "catalog_path": "catalog.json",
  "owners": {},
  "replicas": {}
}
```

`catalog_path` is relative to the config file. The sidecar is named
`library.json.publisher.json` when the config is `library.json`:

```json
{
  "schema": "mneme.library.publisher.v1",
  "peers": [
    {
      "device_id": "peer-device",
      "transport": "local_directory",
      "inbox": "/absolute/trusted/peer-inbox",
      "receive_inbox": "/absolute/trusted/local-inbox"
    }
  ],
  "sources": {},
  "outbox": [],
  "snapshot": {
    "mnemed": "/absolute/bin/mnemed",
    "remote_url": "http://127.0.0.1:18766"
  }
}
```

On each device, `inbox` is the trusted peer's destination and `receive_inbox` is
its local incoming directory for that peer. Do not use world-writable inboxes.

The peer list controls delivery, not query access. Configure explicit `owners`,
`owner_routes` or `replicas` MCP endpoints in the runtime config for reads. Local
registration records exact owner routes by device/database ID; snapshot capture
prefers those routes to the legacy per-device `owners` endpoint. Endpoint URLs and
token variable names are never sent in descriptors. A peer may know a project
without having a usable route yet.

## Enroll and publish a project

Get the native database ID from the running owner or its verified setup receipt.
Then explicitly enroll that project:

```sh
python3 integrations/library/library.py --config /absolute/private/library.json \
  enroll --project-root /absolute/project --db-id VERIFIED_DATABASE_ID \
  --database project --owner-endpoint http://127.0.0.1:18765
python3 integrations/library/library.py --config /absolute/private/library.json status
```

Private/isolated projects are excluded. Reading the library or repeating local
setup cannot promote an entry into sharing. Identity conflicts and withdrawals
require review; `--reenroll` explicitly permits restoring a withdrawn project.

Use the project ID shown by `status`:

```sh
python3 integrations/library/library.py --config /absolute/private/library.json \
  publish --project-id PROJECT_ID
python3 integrations/library/library.py --config /absolute/private/library.json once
```

`publish` asks the existing owner for a native snapshot. Add `--bundle PATH` to
publish an already-created native bundle instead. `once` snapshots due projects
at 15-minute intervals, retries pending delivery and receives local inbox files.
Run it from a timer; [systemd](systemd/) and [launchd](launchd/) contain templates,
not installed services. `sync` retries outbox delivery without taking snapshots.

A copied archive is not completed delivery. The receiver validates its file
inventory, identity and hashes before acknowledging it; the sender validates that
acknowledgment before recording a replica. Offline peers remain visibly pending.

## Serve received replicas

On the receiving device:

```sh
python3 integrations/library/library.py --config /absolute/private/library.json \
  serve --mcp /absolute/bin/mneme-mcp --http 127.0.0.1:18767
```

This starts a dedicated read-only host with generation-pinned aliases and paths.
Run it as a foreground service, separately from the publisher timer. Configure its
endpoint in the library's replica routes; do not substitute a generic host with
different aliases.

For a persistent replica host, the sidecar may provide stop/start argv arrays:

```json
"serving": {
  "stop": ["/absolute/bin/stop-replica-host"],
  "start": ["/absolute/bin/start-replica-host"]
}
```

These run without a shell. Without them, the receiver assumes no persistent host
is open. A failed restart leaves the selected generation intact and reports an
operational error; restart the host before reading.

## SSH transport

Replace a peer's delivery fields with:

```json
{"device_id":"peer-device","transport":"ssh","host":"user@remote-host","port":22}
```

Provision SSH access and host keys first. The remote account must have
`~/.local/bin/mneme-library-receive`, invoking this helper's `receive` with a locally
fixed config and authenticated peer:

```sh
#!/bin/sh
exec python3 /absolute/source/integrations/library/library.py \
  --config /absolute/private/library.json receive --authenticated-peer local-device
```

The sender runs exactly `exec ~/.local/bin/mneme-library-receive` over SSH with
batch mode and strict host-key checking. It cannot choose the receiver config or
peer identity. Keep stdout for the receiver's bounded JSON acknowledgment.

## Retention and withdrawal

Native owner bundles retain the two newest generations plus all durable outbox
references. Receiver catalogs retain current/previous immutable generations.
Cleanup touches only recorded, identity-verified bundles, never unknown directories;
caller-provided `--bundle` data is not registered for cleanup. `status` reports
replica age, so readers can recognize stale snapshots.

To stop publication:

```sh
python3 integrations/library/library.py --config /absolute/private/library.json \
  withdraw --project-id PROJECT_ID
```

Withdrawal is not secure erasure. Restart previously opened replica hosts to
remove old aliases, and explicitly delete retained filesystem copies if erasure
is intended.

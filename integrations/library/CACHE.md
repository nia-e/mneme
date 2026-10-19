# Loaded-store offline caches

`cache.py` populates the existing read-only library replicas **after a successful
canonical memory read**. Catalog discovery, selection, status, writes and cached
replies do not request a copy. It does not use the eager publisher's `once` loop.

Enable population on a store in native configuration or its discovered catalog:

```toml
cache = { library = "offline", mode = "owner-first", populate = { refresh_after_secs = 900, max_snapshot_bytes = 134217728 } }
```

Omitting `populate` disables population without removing an existing replica.
Refresh accepts 60–86400 seconds; a snapshot payload accepts 1–512 MiB. The local
library definition remains device-local. Discovered metadata cannot install an
executable, credential, local path or SSH command.

## What is retained

The native owner makes a coherent database snapshot with its portable `fs://`
body files. The closed file inventory has sizes and SHA-256 hashes. Text and
provenance containing paths, URLs or links remain text: no linked file is followed
or copied. An unsupported, missing or symlinked body refuses the whole snapshot;
it is not silently omitted.

One private demand receipt per canonical database ID records the exact selection
request, digest and discovery witness, never the query, memory payload or token.
Native `mnemed --json cache plan --demand /absolute/path.json` replays that request
**offline**. The worker checks it before capture, after transfer, and immediately
before selecting the new generation. Changed policy or binding means defer, not
mint a fresh grant. Receipts expire after 24 hours; using the store again renews
its demand. Discovery receipts remain workspace/request-bound: a previously
unseen workspace does not become authorized offline because another workspace
cached the same database.

Each immutable generation contains `bundle/`, a separate `serve/` copy and
`cache-binding.json`. The latter records the verified selection alongside its
content. Bundle hashes establish retained content identity. The serving database
is deliberately a separate backend-openable copy and may acquire native backend
metadata; it is not claimed to stay byte-identical after opening. The normal
read-only replica host still checks bundle integrity before opening generations.

Owner-first reads can use a verified advertised local replica on an availability
failure, with snapshot provenance. Authentication, identity and protocol failures
are not permission to silently switch source. Writes never target replicas.

## Local consumer

Ship `cache.py`, `library.py`, `host_files.py` and `host_process.py` from one reviewed
bundle. Python 3.9+ and Unix are required. Configuration and state are ordinary
current-user files, mode 0600, in mode-0700 directories; Nix/Home Manager symlinks
must be materialized as reviewed local files rather than passed to this helper.

Create the normal [library runtime config](README.md#configure-optional-sharing)
and publisher sidecar, with an allowed peer for each canonical owner device and
fixed local `serving.stop` / `serving.start` command arrays. These commands must
control only the dedicated read-only replica host. Start must be idempotent.
Configure that host with `library.py ... serve`, not a writable general server.

A separate worker config can be:

```json
{
  "schema": "mneme.cache-worker.v1",
  "native_cli": "/absolute/runtime/bin/mnemed",
  "library_config": "/absolute/private/offline/library.json",
  "peers": [
    {
      "device_id": "owner-device",
      "transport": "ssh",
      "host": "user@owner-host",
      "port": 22,
      "owner_urls": ["ssh://user@owner-host"]
    }
  ],
  "max_disk_bytes": 2147483648,
  "max_run_secs": 120,
  "max_transfer_bytes": 536870912
}
```

`owner_urls` must match the native owner's URL **exactly**, including any trailing
slash. It pins that route to the authenticated device, not merely to a hostname
claimed by an archive. For a local owner replace the SSH fields with
`"transport": "local", "export_config": "/absolute/private/cache-exporter.json"`.
The fixed map is local operator machinery, separate from the discovered catalog.

Run one bounded pass, or invoke it from a device-local timer:

```sh
/absolute/python3 /absolute/runtime/lib/cache.py \
  --config /absolute/private/cache-worker.json once
```

The helper derives `cache-demand/` from the selected library catalog's parent;
it does not enumerate stores or contact an owner without an eligible receipt.
One worker runs at a time, with at most one transfer attempt per pass. Errors are
best-effort `deferred` results with a phase, not failures of the original read.
A 60-second retry backoff and oldest-attempt-first order prevent a busy owner from
permanently starving the other loaded stores. `idle` is not proof that every store
has a usable cache. `serving_restarted` repairs a prior start failure without
requesting another cold snapshot; a successful start command is not an independent
server-health measurement.

## Fixed owner exporter

Install `~/.local/bin/mneme-cache-export` on the canonical owner's authenticated
SSH account. Its contents must run only this fixed, locally configured command:

```sh
exec /absolute/python3 /absolute/runtime/lib/cache.py \
  --config /absolute/private/cache-exporter.json export
```

The client always invokes that command with BatchMode and strict host-key
checking. It sends closed JSON on stdin, with schema
`mneme.cache-export-request.v1`, `request_id` (32 lowercase hexadecimal digits),
`db_id`, `database` and `max_snapshot_bytes`. No caller-supplied route, endpoint,
path, command or remote credential is accepted.

The owner's configuration pins each allowed identity:

```json
{
  "schema": "mneme.cache-exporter.v1",
  "device_id": "owner-device",
  "stores": {
    "01ARZ3NDEKTSV4RRFFQ69G5FAV": {
      "database": "project",
      "endpoint": {"url": "http://127.0.0.1:18765"},
      "native_cli": "/absolute/runtime/bin/mnemed",
      "snapshot_root": "/absolute/owner/database-parent/snapshots",
      "descriptor_revision": 1
    }
  },
  "max_disk_bytes": 2147483648,
  "max_snapshot_bytes": 536870912,
  "max_run_secs": 120
}
```

`endpoint.token_env` may name a locally supplied credential variable; its value
is never sent in a demand, request, descriptor or diagnostic. Only numeric
loopback HTTP endpoints are admitted. Native `snapshot create --expected-db-id`
checks identity before cold admission. Busy owners defer rather than falling back
to copying a live database.

Descriptors use `store_<db_id>` and the database ID as their stable display name.
The exporter retains a private `.descriptors.json` history beside its config.
Changing the alias or other descriptor content requires a higher explicit
`descriptor_revision`; unchanged content keeps its revision. Do not discard this
history to evade an identity conflict. A Pi exporter cannot impersonate a Mac
owner: each consumer pulls from the authenticated peer that actually owns the
canonical database.

## Bounds, failure and recovery

- Worker time limit: 1–120 seconds, default 120, shared across planning, transport,
  checks and publication. Subprocess output is drained with a pre-allocation byte
  cap, not buffered without limit. Filesystem calls have ordinary OS latency;
  byte/file limits and repeated deadline checks bound application work, not a
  hard real-time guarantee against a wedged filesystem.
- Transfer ceiling: 1–512 MiB including archive overhead, default 512 MiB. Two MiB
  are reserved for archive metadata, so a very small transfer ceiling can refuse
  the minimum one-MiB snapshot. Source snapshot and transfer limits are both
  enforced. Archives are uncompressed, with bounded member count and expansion.
- Disk ceiling: default 2 GiB. The consumer counts retained generations, serving
  copies, stages and unknown residue before reserving archive + stage/bundle +
  serving-copy space. Archive bytes currently live in bounded memory; counting
  them in the disk reserve is deliberately conservative. The exporter reserves
  the native capture's full 512-MiB envelope even when delivery has a lower cap.
- Current and previous generations are retained by the existing receiver. No
  automatic cross-store eviction, reference crawling or unknown-residue deletion
  occurs. At capacity, old verified content remains and new work defers.
- A wrong requested ID, delivery ID, owner, alias, receiver binding, checksum or
  exact selection cannot select the new generation. A rejected post-preparation
  operation can leave an unselected immutable generation; it counts toward disk
  capacity until reviewed. Native capture cleanup removes only the exact bundle
  returned by that capture, never snapshots created by another job.
- If a crash or service-start failure occurs after selecting verified bytes, the
  previous generation files remain. The next due pass can restart serving those
  selected bytes without another capture. Stop/start and native owner/read-only
  host identity still require installed/live validation for a deployment claim.

Do not use `library.py once` for this workflow: that is the separate opt-in eager
publisher. Do not replace only a running server's database file or delete retained
pins to make a capacity error disappear. Preserve evidence, repair the local
configuration/service, and load the store again when fresh demand is needed.

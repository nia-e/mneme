# Separate stores, shared context

Keep memories in different stores when they have different owners, audiences or
purposes. Read several together when that helps the task. Those are separate
choices: a library combines reads, not databases or write destinations.

Mneme already supports named native owners, remote connections and selected
libraries. The [named-store registry](proposals/named-store-registry.md) adds
configurable automatic routing. These interfaces are implemented and component
tested in source; that does not establish what is installed on a device. Existing
integrations require an explicit reviewed configuration replacement.

## A practical split

For personal work alongside company projects, a useful arrangement is:

| Store | Keep here | Current route |
| --- | --- | --- |
| Project | Source-specific facts, decisions and experience | The project's configured owner |
| Company | Shared procedures, terminology and cross-project lessons | A named registry entry for automatic use, or an explicit owner/library read |
| Personal | Personal continuity and deliberately retained collaboration preferences | The configured user owner |
| Misc | Ordinary work outside configured or excluded projects | A separately reviewed device misc binding |

Names are not privacy controls. In particular, calling a store `company` does not
make automatic recording send company material there. The registry's applicability,
purpose and record policy select that destination; overall recording must also be
enabled. A company store can instead be configured as a project's ordinary owner,
under the project contract. Those are distinct arrangements.

## Configure additional stores

The usual user config is `mneme/stores.json`: on Windows, normally
`%APPDATA%\mneme\stores.json`; on Unix, `~/.config/mneme/stores.json`.
`XDG_CONFIG_HOME` overrides the base directory. The project keeps its own owner;
additional entries define a name, purpose, owner connection, verified database ID,
workspace rules and separate read/core/record/maintenance permissions.

`global` and `misc` are ordinary entries, not privileged store types. A company's
purpose can be inline in the same file. A remote personal store selects an SSH
service connection; it is not a local database path to mount or copy.
See the [registry example and field reference](proposals/named-store-registry.md#configuration-shape).

Fresh `mnemed init` discovers the usual registry or accepts `--store-registry PATH`.
Existing integrations do not switch automatically. Registry mode does not combine
itself with legacy global/misc routing; an invalid selected config fails rather
than quietly reviving that old routing.

`mnemed stores` still inventories native CLI owner/library configuration; it does
not inventory this automatic-routing policy. The integration consumes `stores.json`
while sharing the native transport and database-identity checks.

### Keep one owner per store

A native owner can register several aliases for existing stores:

```sh
mneme-mcp --capability-profile curator --http 127.0.0.1:18766 \
  --db company=/absolute/company/memory.db \
  --db project=/absolute/project/memory.db
```

This is an owner-startup example, not an extra process to launch against stores
already held open. Connect to their existing owners instead. The selected
capability profile applies to this whole server; use separate owners when the
stores need different native capabilities. A read-only library can compose their
reads without taking either store's lease.

### Choose the write destination explicitly

These examples assume the owners and aliases already exist:

```sh
# From the enrolled project, use its identity-pinned CLI owner record.
mnemed save "The parser rejects duplicate field names" --operation-id parser-rule-1

# Company memory is an explicit destination, not a different tag on that note.
mnemed --remote http://127.0.0.1:18766 --remote-db company \
  save "Release reviews require two independent approvals" --operation-id release-rule-1

# A remote personal store is still an ordinary native connection.
mnemed --remote ssh://user@memory-host --remote-db user core
```

The first command uses `.mneme/cli.json` and checks its configured database
identity. An explicit `--remote` connection does not itself establish that
enrollment pin; confirm the intended owner and retain SAVE's returned database
identity and operation ID. A saved remote connection is an address book entry,
not extra write authority. See [remote configuration](remote-cli.md) for SSH,
named connections and identity-pinned owner records.

Do not point offline `--db` at a live owner's files, including over a mounted
remote filesystem. SSH carries requests to the owner; it does not move the
database into the client process. Unavailable or mismatched owners must not turn
into a save to whichever other store happens to be reachable.

### Select the reads separately

Use a library containing only the sources intended for that context:

```sh
mnemed library --config /absolute/company/library.json catalog
mnemed library --config /absolute/company/library.json \
  recall-context "release review procedure"
```

Create and enroll that library through the
[library setup workflow](../integrations/library/README.md), rather than copying
live database files. Library responses retain source and generation references;
follow-up reads must retain them too. A library is read-only even when some of
its owners permit writing. Saving still selects one owner.

For a fresh company-only Codex context, an explicit project profile can select an
independent library:

```json
{
  "schema": "mneme.profile.v1",
  "mode": "isolated",
  "library_config": "/absolute/company/library.json"
}
```

Put this at `.mneme/profile.json`; review the independent library's contents and
keep personal sources out of it. Use the profile-aware library launcher, not a
personal library server registered directly in global agent configuration.
An isolated profile excludes ambient personal core and personal fallback. A
`private` profile is different: it excludes sharing, but may still read selected
personal core. See [profile selection](../integrations/codex/README.md#explicit-project-profile-and-library-selection).

Registry startup loads the current project's core first, then selected named
entries with `read.core` enabled, in config order. It does not load every library
store's core. An isolated version-1 profile keeps project core only; version-2
profiles can explicitly allow independent named entries that also permit isolated
use. Legacy hooks retain their older separately configured startup behavior.

## Routing is not information-flow isolation

Separate stores prevent accidental *storage mixing* only to the extent that the
writers choose the right destination. They do not make an agent forget one store
before writing to another. Once personal and company material share a conversation,
model-based scope selection cannot guarantee that the output contains only one.

For a real context boundary, start a fresh isolated session with only the intended
owners and library. Do not fork a personal conversation and call it clean. The
harness's own memory, injected instructions, files and tools also need the matching
boundary; Mneme's profile cannot remove those.

Likewise, disabling automatic recording is not a native access-control rule.
`recording_mode: off` stops the integration's background saves; a separately
exposed curator/operator MCP connection can still write. Use a native `read-only`
profile when that connection must reject writes. Neither profile is authentication
or a sandbox against another process running as the same OS user.

## Purpose and current limits

For an enrolled project, `.mneme/hippocampus.md` can guide automatic capture. It
does not select another owner or grant authority. Named entries use inline
`purpose` text instead; there is no separate Markdown file to keep in sync. The
default personal recipe retains its narrow preference evidence and size limits;
company facts belong in a company/project entry, not that preference lane.

Registry retrieval currently reports `routing_mode: native_only`: native graph
and conditional observation bindings are retained, but the legacy optional
learned routing matcher and new routing-judgment writes are not part of this path.
`concern_mode: warning_only` means the reader may surface a caveat without writing
a native concern record. Maintenance permits guarded same-owner tag stewardship,
not general merging, deletion or cross-owner concern/routing writes. These are
deliberate limits, not evidence that the new routing improves answer quality.

## Outside-project hooks

Project installation owns its combined project-plus-registry session. A separate
device dispatcher covers genuinely unconfigured work; it declines configured,
disabled or isolated project boundaries instead of becoming a second worker.
Replacing a project installation does not replace old device/core hooks.

After retiring those old hooks and checking their state is quiescent, use the
matching installed runtime to preview replacements:

```sh
python /absolute/runtime/lib/registry_config.py prepare \
  --store-registry /absolute/config/stores.json \
  --state-dir /absolute/retained-device-hook-state \
  --reader-codex /absolute/bin/codex \
  --hooks-program /absolute/runtime/lib/hook_launcher.py \
  --hook-config-path /absolute/config/registry-hooks.json
```

The command prints `hook_config`, `global_hooks` and program hashes; it does not
write configuration, register hooks or contact services. Review and save the
hook config at the selected path, then replace/trust the exact returned hook
definitions through the harness. Preserve the previous effort, auth and recording
choices explicitly: `--librarian-effort`, `--reader-auth` and
`--recording-mode automatic` are available; default recording is off. Per-store
record permissions alone do not enable recording. Keep the retained device state
directory and start fresh sessions, without restoring old global/misc workers
alongside the new dispatcher.

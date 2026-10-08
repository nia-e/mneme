# Remote CLI

Use `mnemed` against an existing `mneme-mcp` server over HTTP or SSH. The server
owns the store; the CLI does not open its files or start another writer.

```sh
mnemed --remote http://127.0.0.1:18767 --remote-db project status
mnemed --remote ssh://user@memory-host --user core
mnemed --remote https://memory.example/mcp --remote-db project \
  --remote-token-env MNEME_TOKEN get NODE_ID --body
```

The server must already be running and advertise the operation you need.
Unsupported commands fail rather than falling back to offline storage.

## Store selection

- `--remote-db NAME` selects a server registry entry. The default is `project`,
  or a saved connection's `database`.
- `--user` selects the remote entry `user`.
- `--db` and `MNEME_DB` are offline filesystem paths and conflict with `--remote`.
  Unset `MNEME_DB` before using a remote connection.

Without `--remote`, ordinary commands use configured project, user or device misc
owners as described [below](#default-project-and-user-owners).

### SSH and authentication

SSH forwards to the existing server's loopback HTTP port. Establish ordinary
`ssh USER@HOST` access first: Mneme uses your keys, known hosts and aliases, and
does not accept unknown keys or interactive logins automatically.
`ssh://host:2222` selects the SSH port; `--remote-mcp-port 18766` selects the
server's loopback MCP port (default 18766). The command cleans up its forwarding
child on completion or handled interruption.

`--remote-token-env VAR` reads a bearer token from that environment variable,
not command-line text. Plain HTTP with bearer credentials is allowed only on
loopback; use HTTPS or SSH otherwise. Redirects and ambient HTTP proxies are not
used. Keep credentials out of diagnostics and protect connection files.

## Named connections

Create `$XDG_CONFIG_HOME/mneme/remotes.json`, or `~/.config/mneme/remotes.json`
when `XDG_CONFIG_HOME` is unset:

```json
{
  "version": 1,
  "remotes": {
    "memory-host": {
      "url": "ssh://user@memory-host",
      "database": "user",
      "ssh_mcp_port": 18766
    }
  }
}
```

Run `mnemed --remote memory-host core`. `--user` or `--remote-db NAME` overrides
the saved database. `--remote-config PATH` selects another file. Optional
`token_env` names a credential variable; `--remote-token-env` overrides it.
The CLI never edits this file. It accepts at most 64 entries in 64 KiB.

<a id="default-project-owner"></a>

## Default project and user owners

[Project setup](cli-and-mcp.md#selective-codex-project-memory) writes a
`.mneme/cli.json` record. To connect manually to an existing owner, put this record
at the project root:

```json
{
  "schema": "mneme.cli.owner.v1",
  "url": "http://127.0.0.1:18767",
  "database": "project",
  "db_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV"
}
```

Replace `db_id` with the selected owner's native `databases` result. It must be a
canonical uppercase ULID. The URL is direct HTTP(S) or SSH, not a named connection.
Optional `ssh_mcp_port` and `token_env` have the meanings above. The record is
limited to 8 KiB; unknown fields, explicit null optionals and symlink files or
owner directories are rejected. It is CLI configuration, not Codex hook or
library configuration.

For `mnemed --user …`, use the same schema at
`$XDG_CONFIG_HOME/mneme/cli.json` (default `~/.config/mneme/cli.json`), selecting the
existing user owner and its identity. The CLI never creates or edits these records.

Selection order:

1. `--remote` explicitly selects a connection.
2. `--db` or `MNEME_DB` selects offline storage and bypasses owner records.
3. `--user` selects the private user owner, never project or misc.
4. Otherwise, the nearest project record applies to its subdirectories. Search
   stops at the first `.git` or `.mneme` boundary after checking that boundary's
   own record; nested projects cannot inherit an outer project's owner.
5. Genuinely unconfigured work may use `$XDG_CONFIG_HOME/mneme/misc.json`
   (default `~/.config/mneme/misc.json`). It uses the same schema and must select
   the shared misc owner's `project` alias, never `user`. Selection emits a notice.

Only `misc.json` accepts optional `excluded_roots`, a list of up to 64 unique,
normalized absolute paths within the 8 KiB record. Default misc selection checks
both lexical and canonical ancestry before connecting. A matching exclusion
blocks misc, not an explicitly configured project owner.

An existing `.mneme` boundary, explicit off/private/isolated/reminder setting,
legacy enrollment or owned Codex routing is not absent configuration. Admission
checks enclosing enrollment even across nested Git boundaries. Malformed or
inaccessible configuration, unavailable servers, changed identities and released
owners fail without a local, misc or global fallback. If misc is not configured,
enroll a project or deliberately select `--remote URL` / `--db PATH`.

Before a scoped implicit call, the client checks the server catalog and requires
an advertised `expected_db_id` guard for reads and writes. Older unguarded servers
need an update; checking identity only before the call is insufficient. Explicit
`--remote` alone does not establish an enrollment identity pin; editing commands
require their explicit `--expected-db-id`.

TUI and REPL share owner selection. `tui --config PATH` is an explicit library
view; `tui --demo` is synthetic. Provisioning, upgrades and index replacement use
explicit offline paths instead.

## Save through the selected owner

```sh
mnemed --remote http://127.0.0.1:18767 --remote-db project --json \
  save 'Keep the exact boundary, not just the conclusion' --operation-id review-note-1
mnemed --remote http://127.0.0.1:18767 --remote-db project \
  save --kind episode 'The retry test reproduced a duplicate write' \
  --operation-id retry-scene-1
```

The server must advertise SAVE and already own a current database. Non-core
notes and episode appends require curator; core notes require operator, and
episodes reject core. SAVE never provisions storage or falls back to a legacy
write route.

For an exact retry, retain the same source or operation ID **and all unchanged
authored content**, including body files. The CLI exposes a generated manual ID
before transport. Losing a raw MCP reply containing a generated ID leaves an
ambiguous outcome; JSON-RPC IDs cannot recover it. A transport error after submission
may mean the write committed. Inspect state or replay only the retained exact SAVE
request; no write is retried automatically. See
[SAVE receipts and retry rules](cli-and-mcp.md#save-and-retry).

## Coverage and deliberate differences

Owner mode supports retrieval, body reads, SAVE, list/neighbor pages, walks,
links, curation, online maintenance and snapshot publication, subject to the
advertised profile. Compatibility capture/append and experimental ingest remain
available to their existing callers, not the default SAVE workflow.

`--json` preserves the MCP payload, which can differ from offline command shapes:
for example, `core` returns `{nodes,total,truncated}`. Body reads are bounded text;
invalid UTF-8 is replaced by the server. Prefer 64 KiB chunks and follow
`next_offset`; a larger requested range can hit the response ceiling.

List and neighbor pages retain continuation/completeness metadata. REPL bare
`done`, `abort` and EOF do not train. Only `done USED_ID…` explicitly reflects
visited nodes that contributed. See
[browsing and deliberate learning](cli-and-mcp.md#browse-inspect-and-deliberately-learn).

These offline options have no owner adapter:

| Option or task | Use instead |
| --- | --- |
| `query --bodies` | `get ID --body` for selected hits |
| `recall-context --max-content-bytes` | MCP's fixed 32 KiB context envelope |
| `body --raw` | Bounded text pages with continuation |
| `ingest --body-ref` | Inline text or local `--body-file`, not a server path |
| Provisioning, bootstrap creation, migrations, index replacement | Explicit offline/operator workflow |

For cross-store links, `--to-remote-db NAME` names a registry entry;
`--to-db PATH` remains an offline path. They are mutually exclusive; only user
memory can originate cross-store links.

## ChatGPT and hosted clients

A hosted MCP client needs its own configured transport and authentication.
It does not inherit CLI connection records or Codex hooks. Expose only the
intended databases and review their advertised profiles. An operator connection
is not a read-only share or a per-request approval mechanism.

A private tunnel can forward to loopback HTTP or the router below without
publishing database files. Follow your transport provider's current setup and
access-control documentation. Validate with the catalog and read-only requests,
not a test save. Keep original connection configuration for rollback; revoking
a transport does not undo earlier writes.

### Routing several existing owners

Native server mode can already own several databases. If separate processes
hold their leases, `mneme-mcp --router-config PATH` presents them through one
catalog without opening their files. It defaults to stdio; add
`--http 127.0.0.1:18772` for HTTP. Router mode cannot combine with owner/library
startup flags or a process-wide capability override.

```json
{
  "schema": "mneme.router.config.v1",
  "default_read_db": "misc",
  "databases": [
    {
      "name": "misc", "kind": "owner",
      "url": "http://127.0.0.1:18770/", "db": "project",
      "expected_db_id": "COPY_THE_NATIVE_DATABASE_ID"
    },
    {
      "name": "sample", "kind": "replica",
      "library_config": "replica-only-library.json",
      "project_id": "COPY_THE_ENROLLED_PROJECT_ID",
      "expected_db_id": "COPY_THE_NATIVE_DATABASE_ID"
    }
  ]
}
```

Replace identity placeholders with verified values. Owner endpoints must be
numeric loopback HTTP; optional `token_env` names a credential variable. Replica
config paths are absolute or relative to the router config. Only configured
replica sources are used, with no live-owner fallback; they must advertise native
read-only capability and generation-specific aliases/serving paths.

- `databases {}` inventories aliases, identities, defaults, capabilities and
  snapshot metadata. It is not a fresh health check.
- Reads may omit `db` and use exactly `default_read_db`. Mutations require explicit
  `db`. Invalid, unavailable or replaced selections never fall back. Caller
  `expected_db_id` cannot override the configured pin.
- Results wrap native data in `{db, snapshot?, result}` and preserve error status.
  `databases` returns a process-wide array. IDs and authored provenance are unchanged.
- A fresh replica read chooses the newest configured published copy. Keep its
  complete `snapshot` object for follow-ups; cursor/body continuations require it.
  Expired generations refuse. To restart on a new copy, omit snapshot/cursor and
  reset body or episode offsets to zero. Live routes reject `snapshot`.

The hosted toolset is `core`, `recall_context`, `get`, `list`, `neighbors`, `save`,
`episode`, `link`, `supersede`, `forget`, `status` and `databases`, narrowed by each
owner. Checked `retag` is also forwarded when the writable owner advertises it,
with explicit `db` and `expected_db_id`.

Use `neighbors` for edges: hosted `get(edges:true)` is refused because native
edge expansion can hydrate other registered databases. Stateful walk/reflection,
compatibility writes, cross-database links, lease control and cold maintenance
remain direct-owner workflows. Cached mutations refuse before owner contact.
No failed call is automatically replayed. A changed upstream catalog requires
review and router restart.

Keep CLI owner records pointing at native owners: the router's result wrapper
is not the native-owner format expected by ordinary CLI connections.

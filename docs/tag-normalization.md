# Canonical tag format and explicit migration

New tag inputs use lowercase ASCII letters, digits and single separating dashes.
Literal spaces, underscores, colons, slashes, dots and existing dashes become a
dash; separator runs collapse and edge separators disappear. For example,
`Rust Compiler / 1.82` becomes `rust-compiler-1-82`.

Controls (including tabs/newlines), other punctuation and non-ASCII characters
reject rather than disappear. There is no transliteration or guessed alias:
`C++`, `C#` and `café` do not silently become `c`, `c` and `cafe`. Choose a deliberate
canonical name for new input. Input byte/count bounds apply before normalization.

Exact source-keyed submission proofs retain the submitted spellings. Historical
request digests/codecs, receipts and embedded witnesses are not rewritten to
pretend an old spelling never existed. Exact replay of an already-committed old
request is separate from admitting that spelling as new intent.

## Inventory every configured owner first

Include named stores, inactive configurations, project/user/misc enrollments,
other configured devices, immutable library generations and retained stores.
Different app registrations can own different files even when their aliases or
logical IDs look familiar. In particular, a stdio owner inside an editor is not
an invitation to launch another process against its live database.

Read through the legitimate existing owner. Page `list` with `kind: nodes`,
`status: all` and no tag filter until `next_cursor` is null, retaining the database
identity. This includes historical episode editions and complete tag sets.
Join a complete `kind: touchstones` listing when identifying authored owners.
The semantic tag vocabulary alone omits editions and is insufficient. Such live
pages are not a snapshot: recheck the complete preimage under the offline lease.
An unreachable owner or an unresolved retained file remains unfinished inventory.

## Review a complete lexical plan

Stop future old hooks/jobs, close or explicitly release the legitimate writer,
and establish quiescence before offline work. Preserve queues, paid-work ledgers,
uncertain outcomes and exact submitted requests. Take verified paired backups of
the database and matching body tree, plus the matched old runtime/configuration
and database identity. Do not copy only a live SQLite main file while WAL work
continues.

For an already-confirmed legacy-tag TouchstonesV1 SQLite store:

```sh
mnemed --json tag-format-migration plan \
  --source /absolute/memory.db --expected-db-id DATABASE_ULID
```

An optional reviewed aliases file has this closed shape:

```json
{"aliases":{"C++":"cpp"}}
```

Pass the same `--aliases /absolute/aliases.json` to plan and apply. Aliases match
exact old spellings; they cannot redirect an already-normalizable tag to an
unrelated topic, with one narrow exception below. The plan reports unsupported spellings, protected aliases,
collisions, unused aliases and the exact source/options digest. Review collisions
as merges of spellings, not proof that their meanings were equivalent.

A change such as `Core` to `core` is blocked rather than silently granting special
membership. Explicitly map that noncanonical spelling to a reviewed **nonprotected**
canonical name, for example `{"aliases":{"Core":"historical-core"}}`.
This narrow escape also works for immutable episode editions and touchstone owners:
ordinary retagging remains forbidden, and the format migration preserves their
proofs and witnesses. Literal protected tags such as `core` cannot be redirected;
aliases cannot grant another protected tag, and ordinary `rust` to `java` remapping
still rejects. A changed preimage/options digest requires fresh review, not an
automatic retry. A separate decision to grant protected membership must use an
authorized semantic workflow, never an alias file.

```sh
mnemed --json tag-format-migration apply \
  --source /absolute/memory.db --expected-db-id DATABASE_ULID \
  --expected-plan-sha256 REVIEWED_PLAN_SHA256
```

This command refuses ambient `--db`/`MNEME_DB`, user, remote and declarative-store
selectors. It acquires the ordinary exclusive lease; it does not stop services,
change configuration, resume owners, initialize models or bypass a held lease.

SQLite applies only canonical node tags and their semantic membership rows,
then rotates the mandatory `tag_projection_v2` value in the **same transaction**.
Even a zero-rename store needs the successor policy marker to fence old writers.
Episodes receive canonical tags but remain outside semantic membership. Ordinary
immutable edition/touchstone edit guards are unchanged. Touchstone summary
snapshots exclude tags; their bytes/hashes stay exact. Tag-bearing routing
fingerprints become stale, as after an ordinary retag; old evidence is not rebound.

## Compatibility and recovery

Fresh writable stores require the normalized policy. The named historical
`single-graph-upgrade --target-generation touchstones-v1` target remains frozen:
it preserves old spellings and the legacy tag marker/JSON V5 envelope. Follow
that historical conversion with the separately reviewed lexical transition;
ordinary current admission does not silently adopt it.

JSON uses `--backend json`. Its apply additionally requires a **distinct absent**
`--output /absolute/successor.json`, with its paired successor body tree. Keep the
old source/runtime intact until the explicitly reviewed configuration switch.
The V6 successor envelope fences old normal readers, but cannot stop an old
already-loaded process from overwriting the old source pathname. Never present
this as an in-place JSON stale-writer guarantee.

Retained immutable library generations are historical publications, not writable
owners to mass-edit. Preserve their existing qualified owner/runtime route until
a new library-serving route is separately qualified. A normalized writer must
not rewrite immutable replica evidence just to satisfy a startup check.
The new runtime also refuses the legacy marker on read-only replicas, even when
all their tag spellings already happen to be canonical. Keep their matched
retained serving runtime; do not restart those replica routes with the new binary.
Quiesce the publisher before any runtime switch. A later library cutover must
coordinate a newly published normalized generation, its serving runtime and its
configuration; migrating a writable source does not migrate existing replicas.

Normalized fresh publication uses a distinct recovery-policy binding. An old
interrupted publication directory is refused and retained byte-for-byte, not
adopted or cleaned under the new policy; retain its matching old runtime and
recovery evidence.

After a successful checkpoint, verify identity, canonical tags, index membership
and exact non-tag state with the matched new runtime before deliberate owner
resume. A lost acknowledgement or crash is not an exactly-once receipt. Preserve
all residue and inspect the outcome; dirty or ambiguous state may require the
reviewed backup recovery. Rollback restores the matched runtime/configuration and
database/body pair—not merely an older binary over post-migration writes.

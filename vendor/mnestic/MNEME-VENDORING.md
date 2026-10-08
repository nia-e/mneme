# Mnestic vendoring note

- Source: `mnestic` 0.13.0 from crates.io, whose declared upstream is
  <https://github.com/shuruheel/mnestic>.
- Crates.io archive SHA-256:
  `25c35e2ca626f2e14d3c1ad6500b4dddc5fb6197591d621cc72d3a8743250e43`.
- The archive's `.cargo_vcs_info.json` records upstream commit
  `aecfd1fcf4aecf09675ebba5d78fc61f6c69da6d` at VCS path `cozo-core`.
- License: MPL-2.0. The crates.io archive does not contain the license text, so
  the canonical MPL-2.0 text is carried alongside it as `LICENSE`.
- Local frozen catalog-codec patch: stored relation handles are classified only
  after a bounded allocation-free MessagePack scan and a byte-exact recursive
  v0/v1/struct-map proof. A private frozen wire DTO, literal fixtures, and a
  rich shape fingerprint pin canonical output independently of the live
  `RelationHandle`; semantic F7 authority remains outside the classifier. The
  protocol-facing `rmp`, `rmp-serde`, `serde`, and `serde_derive` dependencies
  are exact-version pins in both manifests. Changing any pin or frozen DTO must
  prove identical fixture bytes or introduce a separately fenced codec
  generation.
- Local indexed-rename patch: `SessionTx::rename_relation` renames every attached index
  relation handle and catalogue key (plain, HNSW, FTS, and both LSH tables),
  rewrites index-manifest base names, preflights all destinations, and preserves
  relation IDs and row data. It also invalidates name-keyed FTS corpus-stat
  cache entries on both sides of a rename, including sequential indexed swaps.
  Focused regression tests live in `src/runtime/tests.rs`.
- Local HNSW removal degree-accounting patch: removal still deletes both rows
  of each stored pair, including pruned rows, but decrements a neighbour's degree
  only when its reverse neighbour-to-removed row was live before deletion. This
  changes no search/connectivity policy, schema or public API, and does not
  backfill counters corrupted by earlier removals.
  `hnsw_remove_counts_only_live_reverse_links_{mem,sqlite}` in
  `src/runtime/hnsw.rs` cover deletion, changed-vector replacement and index-filter
  exit for both asymmetric prune orientations and both-directions-pruned pairs.
  Their deterministic fixture preserves real self-row metadata/vector hashes,
  uses true zero-distance edges between identical vectors, and supplies contiguous
  level-0/-1 towers plus a surviving singleton level--2 entrypoint and valid
  canary. It models the valid paired-row storage contract after pruning, not the
  stochastic builder history or an ANN recall benchmark. Exact numeric degrees
  are checked against live outgoing rows at every level, with nonempty survivor
  towers and nonzero upper-level links, canonical-vector hashes, paired edges and
  entrypoint-target checks. SQLite additionally closes/reopens the owning database,
  compares base/index rows and headers exactly, and repeats the degree/hash/tower
  audit and survivor-search smoke check. Replacement insertion can choose a random
  tower height; assertions do not depend on that choice. These are prospective
  regression coverage, not a claim of deployment or live-store repair.
- Local eligible-set HNSW-build patch: every backend streams a base-relation
  scan through the index filter into the compact flat builder instead of
  retaining a `Vec<Tuple>` containing every unfiltered vector. Peak memory is
  still proportional to the eligible vector slab, graph metadata and adjacency,
  and serialized temporary index rows. SQLite streams those serialized rows
  into its write transaction instead of collecting a second full copy. The
  RocksDB-only off-lock/SST path also retains compact key-plus-digest
  reconciliation evidence rather than the vector-bearing source rows.
  `HnswBuildStats` and the 4,097:1 filtered-lane regression in
  `src/runtime/tests.rs` pin filtered-row retention; they are not an allocator
  or RSS bound.
- Local bounded primary-key-page patch: `PrimaryKeyScan` directly seeks a stored
  relation's physical primary-key interval with a fixed leading prefix, complete
  suffix bounds, direction, and a hard per-call limit. This bypass is necessary:
  CozoScript `:order` materializes a global sorter before applying its limit, and
  an inequality predicate over a stored relation begins with `scan_all` then
  filters, so neither form guarantees bounded late-page work. The API holds the
  relation read lock across metadata lookup and iteration; SQLite additionally
  pushes `LIMIT` into its ordered primary-key SQL instead of stopping only after
  rows cross the Rust iterator boundary. Forward scans remain lazy on every
  backend's native range iterator. Reverse scans are native and lazy for the
  memory, transaction-temp, and SQLite stores used by Mneme; other optional
  stores fail explicitly rather than collecting and reversing. Read-only
  `MultiTransaction::scan_relation_by_primary_key` exposes the same primitive so
  several pages and a final ordinary query can share one storage snapshot; write
  transactions reject it. The mem/SQLite 10,003-row tail-page, boundary,
  direction, validation, and transaction regressions are
  `bounded_primary_key_pages_are_exact_and_limited_in_{mem,sqlite}` in
  `src/runtime/tests.rs`.
- Local bounded read-transaction patch:
  `DbInstance::read_multi_transaction_with_timeout(Duration)` returns a
  dedicated opaque `BoundedReadTransaction` under one immutable monotonic
  deadline anchored before its worker is spawned. The existing
  `MultiTransaction` type, public channels, and commit/abort behavior remain
  source-compatible with the vendored baseline; there is deliberately no
  bounded write variant. A startup handshake returns snapshot-open errors
  directly, idle expiry terminates the worker and drops its snapshot, and
  If consuming `close_and_join` returns, it proves worker/snapshot teardown.
  Ordinary `Drop` now performs that same synchronous close, response drain,
  and exactly-once join. If the destructor returns, panic or early-return paths
  therefore cannot have detached a worker while an outer guard assumes its
  snapshot is gone. Draining through result channel disconnect also prevents
  teardown deadlock when both capacity-one channels are full. Worker panic text
  wins over a generic channel-disconnect error on explicit close; `Drop` is
  non-panicking and necessarily discards that panic provenance.

  The guard is cooperative, not a hard real-time cancellation boundary. It is
  supported only for Mneme's Mem and SQLite backends; optional RocksDB, Sled,
  TiKV, and new-RocksDB builds reject the API at startup rather than pretending
  an uninterruptible backend call is bounded. Audited snapshot and direct-page
  relation-lock waits, parser registry waits, parse/normalization/compile phase
  boundaries, Datalog evaluation, HNSW candidate expansion, result collection,
  direct primary-key pages, and `:sleep` all check the same deadline. A
  per-script Db default and a
  local `:timeout` can tighten but never extend it. Result receive and idle wait
  use that deadline too, so a forgotten handle whose worker is idle at an
  audited receive cannot pin a snapshot forever.
  Bounded queries deliberately skip the Db-global `::running` / `::kill`
  registry so its contended or poisoned mutex cannot pin their snapshot; the
  immutable deadline is their cancellation mechanism.
  Closing a bounded SQLite snapshot finalizes its cached statements first; if
  the connection-pool mutex is still contended at expiry, it closes that spare
  connection instead of pinning joined teardown behind the pool.

  Residual non-preemptible regions are explicit: opening/configuring a SQLite
  connection and individual SQLite/filesystem calls; one Rust in-place output
  sort between its surrounding checks; parser/compiler work within one phase;
  and arbitrary user-registered fixed rules or other extension code that does
  not poll `Poison`. Consequently this API is suitable for audited,
  Mneme-generated read scripts and bounded physical pages, but is not a general
  hostile-query sandbox or a strict latency SLO. Both `close_and_join` and
  `Drop` are completion joins, not deadline-bounded teardown: if they return,
  the worker and snapshot are gone. They wait without a second deadline if a
  worker is already inside one of those residual regions, and may therefore
  wait forever if an uncooperative extension or backend call never returns.
  Handles must be owned and destroyed on a blocking-capable thread. Use
  explicit close when worker panic provenance matters. Regressions in
  `src/runtime/tests.rs` cover nonzero `:sleep` interruption, idle expiry and
  reuse, immutable/default-tightened deadlines, Mem/SQLite startup contention,
  running-query registry contention/poison, direct scans, preserved panic
  causes, overflow, SQLite startup pool retention, a full close-command channel,
  joined teardown under SQLite pool contention, plain Drop during in-flight and
  idle work, full request/result channels, caller-panic preservation during a
  joined unwinding Drop, and worker-panic provenance first observed by explicit
  close.
- Local SQLite concurrency patch: every connection uses a 250 ms
  `busy_timeout`, databases opt into WAL, and write transactions begin with
  `BEGIN IMMEDIATE`. Bounded maintenance can therefore coexist with readers,
  reserves the single writer before doing projection work, and reports genuine
  prolonged contention instead of panicking on a prepared-statement unwrap.
  Mneme's 13 attempts plus capped exponential sleeps impose an exact 4,765 ms
  contention-wait ceiling (excluding statement work and scheduler delay).
  WAL and rollback-mode PRAGMA result rows are checked rather than trusted;
  offline file moves delete sidecars only after a complete checkpoint and a
  confirmed switch to `DELETE` journal mode.
  Mneme's persistent regression exercises an overlapping reader, pins eight
  lifecycle transitions to four `tx_run` statements, and separately pins 64
  same-lane decays to two statements without FTS/HNSW rewrites.
- Mnestic's non-blocking, off-lock HNSW publication is available only to storage
  engines with SST ingest (currently RocksDB). Its Phase-A child now carries a
  durable, relation-ID-bound build marker. A reported build error eagerly removes
  the matching child and its whole key range; after process death, the next create
  of the same `(base relation, index name)` reclaims it before allocating a fresh
  relation ID. Phase D clears the marker in the same transaction that attaches
  the child to the base metadata, preserving data-before-metadata publication.
  The marker is stored in the child relation's description as route-specific
  recovery metadata; it is not an authentication or security boundary.
  Recovery deletes only an exact internal marker on an otherwise unmodified child;
  an unmarked, malformed, or modified collision fails closed. Rocks-only tests
  `rocks_hnsw_reported_error_cleans_ingested_child_and_retries`,
  `rocks_hnsw_reported_phase_a_error_cleans_empty_child_and_retries`,
  `rocks_hnsw_phase_a_kill_is_recovered_after_reopen`,
  `rocks_hnsw_post_ingest_kill_reclaims_old_data_range`, and
  `rocks_hnsw_recovery_rejects_an_unmarked_child` pin those boundaries.
  Remaining limitations are explicit: recovery is retry-triggered rather than a
  startup sweep; unmarked children made by a pre-patch build are not guessed at
  and require operator-directed backup restore or library-level catalogue repair;
  deterministic phase-stop tests model process death but do not issue a real
  `SIGKILL`; and the off-lock protocol assumes one owning `Db` runtime and no
  concurrent base-relation rename/removal. Ordinary row mutations remain supported
  and are reconciled in Phase D. Mneme enables `storage-sqlite`, so none of this is
  currently on its live backend: SQLite builds remain transactionally blocking even
  though scan/filter memory is bounded by the local patch above. The flat HNSW
  builder uses scoped standard threads and does not require `rayon`.
- Mneme omits the optional `rayon` feature. It parallelizes independent Datalog
  rule evaluation, not HNSW construction. Mnestic 0.13 pins Rayon 1.10 while the
  wider workspace uses 1.12; Cargo can resolve both, so this is not a hard
  dependency conflict. Re-enabling it should be a measured query-throughput
  choice rather than an HNSW requirement.
- Because Cargo excludes this crate from the root workspace, run its complete
  locked/offline suite with `make check-vendored-mnestic`.
  The two real non-UTF-8 pathname tests exercise native byte preservation/refusal
  only on filesystems that can create those names. On macOS, fixture creation may
  fail with `EILSEQ` or permission denial; those branches verify the directory is
  unchanged and report the narrower coverage rather than claiming the native
  path was exercised. This is test portability, not relaxed runtime admission.
- Upstream status checked 2026-07-21: 0.13.0 still renames only the base relation
  catalogue handle, so a future version bump must not silently drop this patch.

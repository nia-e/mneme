# Recursive store configuration discovery

Status: implemented and validated in the working tree (2026-10-19). Baseline
`d9f6427`; no live rollout or commit is implied. Operator guide:
[Store discovery](../discovery.md).

## Outcome and authority

A client can configure one verified seed owner with its canonical database ID,
route and `discovery = true`; that owner supplies sparse configuration for itself
and other stores, including further discovery links and per-store activation,
delivery, permission, capture and background policy. Explicit client-local values
win by database identity. Ordinary workspace restrictions and native owner
capabilities remain independent ceilings. Names are labels, not identities.

This work includes source implementation, documentation, disposable tests and
isolated artifact checks. It does not authorize deployment, live configuration
changes, enrolled-store writes, service/hook changes, or automatic announcements.
The earlier create-and-announce proposal remains a separate follow-up: this first
implementation serves explicitly operator-authored catalog metadata. It does not
couple discovery to library snapshots/replication or ordinary memory nodes.

## Settled resolution rules

1. Preserve presence until resolution. Omission is no opinion; explicit false,
   empty lists and explicit clearing remain meaningful. Apply defaults last.
2. A declaration wins over deeper declarations **on the same branch only**.
   Independent branches merge even when their lengths differ. Network response
   order, URL order and global breadth-first depth are not precedence.
3. For each target-ID/setting, walk from seed publishers. The first publisher
   explicitly declaring that setting is an absorbing barrier for that walk:
   record it and do not traverse through it for that setting. Omitted settings
   let the walk continue. A visited set handles cycles without enumerating paths.
   Merge the contributors found on all unblocked branches. A declaration can be
   shadowed on one branch and still contribute through another.
4. Local explicit values override imported contributors. Equal values agree;
   disjoint fields fill gaps. Boolean permission switches can conservatively meet
   at false. Other unequal values remain visible conflicts: do not delete them
   and accidentally recover permissive defaults. Disable the affected operation
   or leave an owner unresolved when a required connection is ambiguous. Treat
   coupled route fields coherently; never build a route from conflicting fragments.
5. Resolve catalogs before activating memory reads/background work. Discovery is
   bounded metadata collection, not activation. A reachable explicit discovery
   opt-in can authorize catalog-only contact; local explicit disable/exclusion
   gates still apply. Retain fetched evidence for the run instead of recursively
   retracting it when final merged policy disables an operation. Report this
   distinction in provenance. Unknown/unanswered publishers are not empty ones.
6. Verify canonical identity on every owner connection; no credential values,
   remotely chosen local secret references, executable configuration or arbitrary
   local-open targets are imported. Device-specific workspace paths require a
   matching client device binding, not universal interpretation of absolute paths.

## Architecture and frozen selection

- `mneme-config`: checked sparse declarations, pure branch-first graph resolver,
  structured provenance/conflicts, local workspace collection and final selection.
  It stays free of network contact and memory-store opening.
- Native MCP owners: a bounded `discovery_catalog` read backed by an explicitly
  configured operator metadata file, with publisher identity and content revision.
  All native profiles may read it; it does not check out the database, acquire
  cold work, infer, reinforce, create receipts or mutate topology. Coordinators
  are not implicitly publishers.
- `mneme-store-client`: one shared bounded acquisition workflow for CLI, configured
  MCP and hook-facing native commands. Share an aggregate deadline/receive budget
  across initialize, catalogs, identity checks and failed reads. Fetch each admitted
  publisher snapshot once per run; bound documents, graph work and provenance.
- A native-owned bounded client metadata receipt freezes imported documents and
  source revisions. Initial resolution may acquire; ordinary replay/revalidation
  never acquires new grants or switches routes. Atomic receipt replacement makes
  refresh explicit and makes old frozen selections refuse/reselect. These are
  client configuration files, not database generations or data migrations.
- Preserve no-discovery serialization/digests where possible. Old clients must
  refuse unsupported discovery syntax rather than silently omit its policy.
- Unavailable refreshes retain usable previous configuration with explicit stale/
  incomplete coverage; fresh unresolved/conflicted policy cannot silently activate.
  Pinned configuration is not a promise of instantaneous remote-policy revocation.

## Operator inspection

Expose the same bounded report through `mnemed config inspect` (human and JSON)
and a TUI configuration inspector. Show effective values, local overrides,
contributing publisher IDs and branch witnesses, shadowed declarations, conflicts,
unavailable publishers and admission/activation separately. Inspector startup
must work without an admitted default or any routable store, and must not trigger
memory reads merely to display configuration. Keep explicit-target isolation.

## Implementation sequence

- [x] Inspect existing config, catalog, client and TUI boundaries; select gates.
- [x] Implement sparse types and pure branch-first resolution with regression tests.
- [x] Add owner metadata catalog publication and guarded transport contract.
- [x] Add shared acquisition, bounded native receipts and exact offline replay.
- [x] Integrate CLI/configured MCP/hook consumers without duplicated policy.
- [x] Add CLI and TUI provenance/conflict inspection.
- [x] Independent review; focused and affected aggregate tests; isolated artifact smoke.
- [x] Update operator documentation and this plan with exact evidence/limitations.

Workers own disjoint modules; cross-module DTO/API changes are coordinated before
integration. Existing uncommitted work must be preserved. No broad cleanup is
planned; source baseline is already committed.

## Required regression cases

- **Same branch:** A -> B -> D: A's explicit D setting wins, B fills omissions.
- **Diamond:** A -> (B, C), both -> D: peer conflicts merge conservatively.
- **Unequal-depth pentagon:** A -> B -> D and A -> C -> E -> D: B and E are peers
  for D, not shorter-path/depth winners.
- **Bypass:** A -> B -> E and A -> C -> E; B's D declaration does not suppress
  E's D declaration reached through C.
- **Collective barriers:** same graph, B and C both declare D; E contributes no
  value for that field because every path is blocked, even without one dominator.
- **Cycles/self-description:** termination, deeper fill, no network-order effect.
- Explicit local true/false/empty overrides; unresolved route and coupled fields;
  conflicting false never becoming enabled by omission/defaulting.
- Unsupported/malformed/oversized publications, identity mismatch, unavailable
  versus genuinely empty catalog, total-byte/deadline exhaustion, partial coverage.
- Imported policy with local exclusions/isolation, device qualification, no secret
  export, and unchanged local-only selection/replay behavior.
- Frozen replay does no network; receipt/local-boundary change invalidates old work;
  failed refresh does not erase retained configuration or mint missing grants.
- TUI inspector no-contact startup, zero/defaultless/blocked stores, narrow screen,
  scrollable provenance, modal input isolation; CLI human/JSON agreement.

## Evidence gates

Initial classes: `public-interface`, `capability`, `transport-package`. Selected
packs: API-01..05, CAP-01..05, DIST-01..05. Source-only work does not satisfy the
isolated installed-artifact floor. Initial selector receipt:
`target/discovery-20261019/gates-initial.json` (local evidence, not portable proof).
Re-run against all actual touched paths before completion. No storage owner,
database format, store lease or data-generation publication changes are intended;
reselect gates if implementation crosses one of those boundaries.

## Completion and evidence boundary

Implemented in `mneme-config::discovery`/workspace/receipt, the native owner's
`discovery_catalog`, the shared `mneme-store-client::discovery` acquisition/cache
workflow, configured MCP startup, CLI select/inspect/call, the TUI config inspector,
and matching Codex/Claude helper replay paths. Full reports stay separate from
compact selection witnesses. Explicit null clears remain distinguishable from
presentation omissions in serialized provenance.

Final current-source checks:

```sh
cargo test --locked -p mnemed
cargo test --locked -p mneme-config -p mneme-store-client -p mneme-mcp-client \
  -p mneme-mcp -p mneme-tui
python3.14 -m unittest discover -s integrations/codex -p 'test_*.py' -q
```

The two Rust aggregates passed 1,060 tests with one ignored; the Python suite ran
1,855 tests with 25 skips and no failures. Owner no-default and no-default+HTTP
feature suites also passed. Tests in `crates/mneme-config/tests/discovery.rs` and
`crates/mneme-store-client/src/discovery/tests.rs` cover the explicit same-branch,
diamond, unequal-depth pentagon, bypass, collective-barrier and cycle cases.
Workspace/cache tests cover incomplete referrals, local privacy changes, stale
receipts, alias collisions and failed-refresh retention. Native owner tests cover
all profiles, released metadata, pre-open validation and original-ID fencing.

Independent resolver, boundary, owner and UI reviews found issues that were fixed
and regression-tested before artifact qualification. A copied cozo+HTTP native
bundle exercised recursive acquisition against disposable real owners over HTTP
and stdio, exact replay offline, explicit refresh, identity refusals and clean
shutdown/lease reopening. Matching copied helpers exercised native acquisition,
child replay and drift refusal with zero canary-publisher contact. Six finite TUI
PTY cases verified no automatic memory reads during config inspection, including
narrow/zero/defaultless/blocked/malformed states and explicit first-route opening.

The complete local gate report, exact source/binary hashes, commands and receipts
are in `target/discovery-20261019/README.md`. These are local evidence, not portable
checked-in artifacts; the test/source paths above are the durable locators.
API-01..05, CAP-01..05 and DIST-01..05 are the selected gates. No storage-format or
migration class was added. No live store, service, configuration or hook was
changed. Automatic create-and-announce, live device rollout, Windows execution
and SSH-host validation remain outside this implementation/verification scope.

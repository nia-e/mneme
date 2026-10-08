# Repository sync contract

This is the committed-tree `repo-sync-v1` contract. It separates a deterministic,
reviewable plan from live database authority. The offline validator proves the Git
cut, evidence, manifest history, action shape, and hard bounds. It cannot prove a
live database precondition.

## Safety boundary

There are exactly three modes:

| mode | manifest | DB writes |
|---|---|---|
| `greenfield` | absent | native `mnemed bootstrap-create` only |
| `managed_refresh_proposal` | valid and historically verified | forbidden; proposal only |
| `brownfield_proposal` | absent | forbidden; proposal only |

Ordinary Mneme APIs do not expose an atomic managed catalog, effective-policy
attestation, mutation epoch, idempotent external key, owned edge-assertion layer, or
CAS replace/retire operation. Therefore **repo-sync-v1 planning is non-mutating in every mode**.
Offline sidecars and journals are review/format contracts only; they cannot authorize
or attest a live write. The sole exception is native greenfield creation: after exact
review approval, `mnemed bootstrap-create` revalidates the contract inside its trust
boundary and creates an absent generation atomically. Managed and brownfield modes
remain strictly non-mutating proposals.

All generated JSON sidecars live under `.mneme/bootstrap`. Local inventory and plan
publication fully writes and fsyncs a same-directory temporary inode, atomically
hard-links it to an absent final name, removes the temporary name, and fsyncs the
directory. Readers therefore see either no final artifact or the complete artifact;
portable rename-overwrite is never used. An exact-byte retry is idempotent, while a
different artifact at an existing path is never clobbered. Changed artifacts use a
new path. Native greenfield manifest/receipt publication uses an authenticated
no-clobber generation boundary; shell redirection is not a publication rule.

The logical project manifest name remains `.mneme/bootstrap/manifest.json`. For that
canonical name only, inventory resolves an activated `.mneme/current` selector to the
selected generation's `bootstrap/manifest.json` while reporting the logical name.
Custom manifest paths remain literal only before a native catalog exists, for tests or
brownfield proposals; they cannot bypass an activated/interrupted catalog. An
orphan/malformed selector, conventional plus activated ambiguity, or a
missing/malformed selected DB/manifest fails closed; native creation does not publish
a duplicate root manifest.

The conventional `.mneme` parent used by default project discovery must itself be a
real directory. Resolution rejects a symlink before inspecting or creating children,
so repository-controlled path redirection cannot move the database/body tree outside
the repository. Explicit non-conventional database paths remain operator-owned.

### Native non-creating inspection

`mnemed --json bootstrap-inspect --root <repo>` is the sole mode-selection probe for
this workflow. Its schema-v1 `repo-sync-v1-bootstrap-inspection` result distinguishes
`greenfield_absent`, `active_managed`, `conventional_unmanaged`,
`interrupted_activation`, and fail-closed `blocked_malformed`, `blocked_orphan`, or
`blocked_symlink` layouts. It reports an existing DB id/generation only when bounded
artifacts make that identity safely inspectable, and emits fresh suggested target DB
and operation ULIDs only for a greenfield-absent layout.

Inspection never invokes the normal store resolver/open path, creates `.mneme`, opens
an embedder, or creates a lease inode. It may open an already-present lease inode
read-only and take a nonblocking shared probe to report `free` versus `held`; the
observation is advisory and mutation-free. Native generation classification verifies
the colocated key, receipt MAC/bindings, and canonical manifest hash without opening
the database. Conventional SQLite identity remains unknown rather than opening Cozo
to discover it. Every result is `authoritative:false` and `retry_safe:true`: native
create or a future managed apply still reacquires its exclusive lease and rechecks all
live preconditions.

An interrupted activation permits only exact native retry with the original reviewed
plan, approval, and operation id. A blocked result includes an actionable blocker and
does not authorize falling back to conventional open, choosing a generation by
recency, or deleting an artifact. A held lease similarly blocks mutation until the
owner releases it, without changing the underlying layout classification.

## Canonical construction and evidence

Canonical JSON is UTF-8 with sorted object keys, compact separators, ASCII escapes,
and no NaN or infinity. SHA-256 hashes canonical bytes. Text is Unicode NFC; every
Unicode line boundary becomes LF; spaces and tabs at line ends and surrounding blank
lines are removed. Synthesized text rejects terminal controls other than canonical
multiline boundaries/tabs. Repository and sidecar paths containing C0, C1, or DEL
control characters fail closed even when Git can represent them.

A deterministic high-confidence secret scan runs over every selected blob and final
plan/review/approval/journal/manifest artifact. A hit makes that input ineligible or
blocks publication without echoing the candidate value. This is defense in depth,
not comprehensive data-loss prevention; the human review remains responsible for
contextual sensitive material and unresolved `sensitive` findings block apply.

Every evidence reference is exact and span-bearing:

```json
{"path":"docs/design.md","blob":"<git oid>","sha256":"<bytes sha256>","span":"10:24"}
```

The validator proves the path/blob at the relevant commit, SHA-256 of its bytes,
strict UTF-8, and the inclusive line span. A node content hash covers exactly
`schema_version`, `namespace`, stable `key`, normalized `summary`, normalized
`claim`, and sorted evidence identities `{blob,sha256,span}`. Paths are excluded so
a unique same-blob path rename does not manufacture a new semantic version; paths
remain covered by the plan and manifest hashes. The validator recomputes the rename
and refuses zero-to-many, many-to-one, or many-to-many guesses.

A materialization hash additionally covers body SHA-256, tags, initial status,
stability, and confidence. An edge assertion hash covers its stable key, logical
endpoints, kind, initial weight, and evidence identities. Bootstrap may assert only
`associative`, `supersedes`, or `derived-from`; `Transition` is learned route state and
cannot be minted from repository prose. Edge assertions are unique by directed
endpoint pair regardless of kind; reverse duplicate associative assertions are
forbidden.

### Complete inventory binding

Inventory schema v2 accepts committed `HEAD` only. It resolves one exact
`HEAD^{commit}` OID, disables Git replacement refs and graft-file substitution, uses
the OID rather than symbolic HEAD for all tree/object reads, and rechecks HEAD plus
the resolved tree after construction. A caller cannot select another revision.

Every bounded path/content eligibility check runs before ranking. Only then are safe
files ranked and selected, preventing poisoned high-priority inputs from consuming a
limited selection window. Every committed tree entry appears exactly once: either in
the selected `files` array or the complete `exclusions` array. Each exclusion binds
path, blob OID, size, mode, type, reason, and SHA-256 when content was read. The
artifact binds `exclusions_sha256` and its own canonical `inventory_hash`; hard entry,
scan-byte, and artifact-byte limits fail closed rather than truncating authority.

A plan binds the inventory's repository-relative path and canonical hash. The plan
builder materializes every selected source from that artifact and accepts only a
decision for each exact selected path. It materializes the exclusion count/digest;
callers cannot supply a shorter source or exclusion list. Validation reloads and
reconstructs the complete artifact from Git and rejects even a self-consistent forgery.

## Hard policy

Plans name `policy_version: "repo-sync-v1-default"`. Caller-supplied limits may only
lower these maxima:

- 20 node records, 10 changed nodes in a managed proposal, and 40 edge records;
- 16 KiB per body, 2 KiB for the direct-loaded compatibility core body, out-degree 8, and in-degree
  12 over retained plus proposed edges;
- 500 selected inventory sources, 100,000 complete-tree entries/exclusions, a 1 GiB
  eligibility scan ceiling, and a 32 MiB inventory artifact;
- 16 sources per record and 16 tags per node;
- 1 KiB summaries, 128-byte tags, 100 findings, and 500 brownfield dispositions;
- 1 MiB per source blob;
- 2 MiB each for draft/plan, manifest, and recovery-journal JSON; 16 KiB for the
  approval artifact and 256 UTF-8 bytes for reviewer identity.

Raising a JSON `limits` value never raises policy. A future explicit trusted policy
version may do so; hand-editing this plan may not.

## Plan

The builder owns normalized bodies, hashes, ordering, policy version, and
`post_manifest_delta`:

```json
{
  "schema_version":1,
  "namespace":"repo-sync-v1",
  "policy_version":"repo-sync-v1-default",
  "mode":"greenfield",
  "target":{"db_id":"<ulid>","expected_empty":true},
  "repo":{"head":"<commit>","tree":"<tree>","object_format":"sha1","dirty_digest":null},
  "base":{"manifest_path":".mneme/bootstrap/manifest.json","manifest_generation":0,"manifest_hash":null},
  "inventory":{"path":".mneme/bootstrap/inventory-001.json","sha256":"<inventory hash>"},
  "limits":{"nodes":20,"edges":40,"bytes_per_body":16384,"max_out_degree":8,"max_in_degree":12},
  "sources":[
    {"path":"README.md","blob":"<oid>","sha256":"<sha256>","bytes":1200,
     "class":"overview","decision":"evidence"}
  ],
  "nodes":[], "edges":[], "brownfield_dispositions":[],
  "adversarial_findings":[{"kind":"other","status":"resolved",
    "detail":"Adversarial review completed.","disposition":"No blocker found.",
    "sources":[]}],
  "exclusions":{"count":12,"sha256":"<complete exclusion array hash>"},
  "post_manifest_delta":{}, "plan_hash":"<builder output>"
}
```

The human-authored draft contains the same inventory binding plus a
`source_decisions` object whose keys exactly equal all selected inventory paths. It
does not contain `sources` or `exclusions`; those fields are builder-owned.

Greenfield node action `ingest` and edge action `link` are the only action shapes the
native create command may execute after exact approval. The JSON actions themselves
are not write authority. Every link endpoint must be an ingest in the same plan.

Managed proposal node actions are:

- `noop`: expectation-only current id/content/materialization hashes plus current
  evidence; valid only for unchanged bytes or a uniquely proven path rename;
- `ingest_proposal`: new managed concept;
- `replace_proposal`: expected current id/content hash plus successor content;
- `retirement_proposal`: bodyless exact current hashes, reason, and all owned source
  paths, all absent with no rename candidate.

Managed edge actions mirror those as `noop`, `link_proposal`, `replace_proposal`,
and bodyless `retirement_proposal`. Brownfield accepts only node
`ingest_proposal`/`adoption_proposal` and edge `link_proposal`. Proposal endpoints
may participate only in proposal edges; no proposal is executable.

Every changed, renamed, or deleted manifest-owned record must appear in a managed
proposal. Omitting an affected record invalidates the plan. Adoption maps exactly one
legacy record to one proposed managed key. A legacy disposition binds its id, status,
exact summary SHA-256, and a SHA-256 of the complete canonical live record; re-read
all of them before a future authorized adoption.

`post_manifest_delta` is a builder-owned sorted list of `{key,action}` pairs. Only a
greenfield delta has `publish:true` and `next_generation:1`. Proposal deltas have
`publish:false` and no next generation. Greenfield `publish:true` describes the
postcondition native create may produce; it is not independently write authority.

## Recovery body

The builder emits this exact ordered header followed by the normalized claim:

```text
--- mneme-repo-sync
namespace: repo-sync-v1
key: project:overview
content-sha256: <node content hash>
source-commit: <reviewed plan HEAD>
extractor-version: 1
source-state: git-committed
content-trust: untrusted-evidence
---
```

Always tag materialized nodes `repo-sync-v1`. Git commitment proves source state, not
content trust. Greenfield contains exactly one active compatibility `core` node; a
core candidate is invalid, and no node citing agent instruction files may be
auto-core. In the canonical core redesign, the tag becomes migration input for an
explicit membership and is not a trust bit.

Adversarial findings carry explicit `resolved | unresolved` status. At least one entry
is required to distinguish a completed pass with no additional objections from a pass
that never happened; use a resolved `other` entry for the former. Any unresolved
`conflict`, `ambiguity`, `injection`, `ownership`, or `sensitive` finding blocks future
native creation.

## Human review gate

Render the canonical material diff with `render_plan_review.py`. The renderer includes
the complete plan and binds it to the plan hash, target DB, and source commit. A human
reviews that exact rendering and creates an approval artifact with exact fields:

```json
{"schema_version":1,"namespace":"repo-sync-v1","plan_hash":"<sha256>",
 "rendered_review_sha256":"<sha256>","reviewer_id":"<auditable identity>",
 "decision":"approved"}
```

The native create command requires this artifact and binds its canonical hash into the
authenticated result. Approval of a different plan/rendering, a blank reviewer, or a
non-approved decision is invalid. This is an auditable human gate, not a cryptographic
identity proof; deployments that need non-repudiation must add signed approvals.

## Reconstructible manifest

The sidecar is tied to one Mneme `db_id`. It records the last applied Git cut and
exact current evidence files. Each stable node or edge key holds a `current` version
and sorted `history`:

```json
{
  "schema_version":1, "namespace":"repo-sync-v1", "db_id":"<ulid>",
  "generation":1,
  "repo":{"head":"<commit>","tree":"<tree>","object_format":"sha1","dirty_digest":null},
  "files":{"README.md":{"blob":"<oid>","sha256":"<sha256>"}},
  "nodes":{
    "project:overview":{
      "current":{
        "node_id":"<ulid>","content_hash":"<sha256>","materialization_hash":"<sha256>",
        "summary":"...","claim":"...","tags":["core","repo-sync-v1"],
        "status":"active","stability":0.8,"confidence":0.9,
        "body_source_commit":"<commit>","evidence_commit":"<commit>",
        "evidence":[{"path":"README.md","blob":"<oid>","sha256":"<sha256>","span":"1:8"}],
        "state":"current"
      },
      "history":[]
    }
  },
  "edges":{},
  "applied_plan_hash":"<sha256>"
}
```

Historical node versions use `superseded` or `retired`; historical edge assertions
use `replaced` or `retired`. Each retains physical ids, content/assertion hashes,
canonical source material, its evidence commit, and the body source commit. Validation
proves every historical commit is an ancestor of the manifest cut and every cited
path/blob/SHA/span at that cut. Current edges store physical endpoint ids and must
target the current node versions. This avoids laundering ownership or orphaning a
superseded candidate when native managed apply eventually exists.

Current `build_manifest.py` deliberately refuses construction. A manifest that claims
physical ids or publication cannot be derived safely from caller-edited JSON. Native
`bootstrap-create` constructs it from committed native results.

## Recovery journal

The journal schema remains a deterministic format/reference for future native managed
refresh. Its operation ordering is `verified*`, at most one
`started`/`acked`/`failed` boundary, then `planned*`; an acked boundary blocks later
work until verified. An edge cannot be acknowledged before both endpoint ingests are
verified, and its ids must equal their node results.

Current `build_journal.py` deliberately refuses construction. Offline validation of a
journal proves only format consistency: it does not prove monotonic history, a live
row, an absent target, or that a process actually returned an id. A caller-edited
`verified` state is never publication authority.

The live projection and any recovery record must instead be produced inside the native
trust boundary. Never infer identity from search, summary similarity, or a manually
assembled journal.

## Native bootstrap-create receipt

The native operation targets an absent, disposable generation, enforces a fixed
topology-off policy, constructs and verifies the projection privately, and atomically
activates it. It owns idempotency and publication recovery.

Its authenticated receipt semantically binds at least:

- the plan hash and canonical human-approval hash;
- the DB id plus storage incarnation/generation;
- the complete effective-policy fingerprint, with every automatic topology writer off;
- an idempotent operation id and absent-target precondition;
- the exact final projection digest and physical node/edge id mapping;
- the manifest hash and publication/activation state; and
- native authentication/key identity sufficient to reject off-path artifact changes,
  mismatched-operation replay, or a receipt minted without the colocated key.

This is not an offline caller-authored schema. No `--receipt path.json` escape hatch is
valid: the receipt is an authenticated native result verified through the same trust
boundary that owns the DB. Offline validators still return `apply_allowed:false`
because structural validity is not authorization; native create independently checks
the exact approved plan. Journal/manifest builders remain fail-closed.

Authentication is scoped to internally consistent historical generation artifacts.
The HMAC key is stored beside the graph, so it detects accidental/off-path tamper but
does not resist a malicious same-UID process that can replace both artifacts and key.
An exact retry of an already active generation verifies its historical
receipt/manifest, DB identity, and policy fingerprint; it does not revalidate the
current database/body projection after legitimate later mutation.

Exact retry also collects abandoned private generation-build directories for that
operation id before recovery. Matching is operation-scoped and bounded to a real
generation catalog; work for other operation ids is retained, while symlink or
non-directory matches fail closed instead of being followed or removed.

## Mandatory native boundary for mutating refresh

Managed refresh stays read-only until Mneme provides all of:

- exact paginated `managed_snapshot(owner)` with DB id and mutation epoch;
- one idempotent `managed_apply(plan, expected_epoch, operation_id)` entry point;
- unique `(owner,key,content_hash)` lookup/reservation and exact retry semantics;
- transactional replace that makes the successor current, retains predecessor
  ownership, and tombstones it as non-promotable rather than making it a candidate;
- source-owned edge assertions separate from adaptive learned edge rows, with physical
  reprojection when endpoint versions change;
- exact authorized retire CAS distinct from a proposal;
- explicit stamping of the reviewed source commit for the whole apply.

Until then, calling a CLI/MCP loop a safe stale-graph refresher is false.

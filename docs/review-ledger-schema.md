# Review ledger schema v1

Use a review ledger to record a disposition for every finding in a committed
review report and bind each response to exact committed evidence. This reference
is for ledger authors and reviewers using `tools/review_ledger_verify.py`.

`mneme.review-ledger.v1` binds one source report, one counter-report (the response
to that review), and their finding/evidence locators. A locator is a literal text
marker that must occur exactly once in the cited file.

The verifier checks the format, finding coverage and committed bytes. It does not
judge the response or execute its evidence: no tests, installed-binary inspection,
live-store access, evaluations, missing-object fetches or external calls.

## Command

```sh
python3 tools/review_ledger_verify.py \
  --repo /path/to/repository \
  --ledger /path/to/review-ledger.json
```

Add `--json` for one machine-readable result object. `--repo` and `--ledger` are
resolved as ordinary process paths. Every path *inside* the ledger is instead a
normalized repository-relative Git path.

Exit status is stable:

- `0`: the ledger and every local Git evidence check are valid;
- `1`: schema, invariant, coverage, commit-object, blob, hash, or locator evidence
  failed;
- `2`: invocation, JSON decoding/parsing, repository access, Git execution, or I/O
  failed.

A missing declared commit or blob is an evidence failure (`1`). An inaccessible or
malfunctioning repository is an operational failure (`2`). Diagnostics are capped at
50 detailed errors and report how many additional errors were omitted.

## Shape

This abbreviated example shows the field structure; angle-bracket values are
placeholders and are not valid ledger values.

```json
{
  "schema": "mneme.review-ledger.v1",
  "artifact": {
    "path": "docs/independent-review.md",
    "artifact_commit": "<full commit SHA containing the frozen review>",
    "git_blob_sha256": "<64 lowercase hex>"
  },
  "response": {
    "path": "docs/independent-review-response.md",
    "response_commit": "<full commit SHA containing the counter-report>",
    "git_blob_sha256": "<64 lowercase hex>"
  },
  "reviewed_commit": "<full commit SHA of the code that was reviewed>",
  "source_findings": [
    {
      "id": "F1",
      "locator": {
        "literal": "### F1 — Unsafe publication window",
        "literal_sha256": "<64 lowercase hex>"
      }
    }
  ],
  "entries": [
    {
      "finding_id": "F1",
      "disposition": "fixed",
      "closure_tier": "component_test",
      "summary": "Publication and rollback now share one transaction.",
      "response_locator": {
        "literal": "### F1 — Unsafe publication window",
        "literal_sha256": "<64 lowercase hex>"
      },
      "evidence": [
        {
          "role": "source_proof",
          "path": "crates/example/src/lib.rs",
          "commit": "<full commit SHA>",
          "git_blob_sha256": "<64 lowercase hex>",
          "locator": {
            "literal": "fn publish_atomically(",
            "literal_sha256": "<64 lowercase hex>"
          },
          "description": "The implementation uses the atomic publication path."
        },
        {
          "role": "component_test",
          "path": "crates/example/tests/publication.rs",
          "commit": "<full commit SHA>",
          "git_blob_sha256": "<64 lowercase hex>",
          "locator": {
            "literal": "fn failed_publish_leaves_no_partial_state()",
            "literal_sha256": "<64 lowercase hex>"
          },
          "description": "Focused crash-boundary regression test."
        }
      ]
    }
  ]
}
```

Every object is closed: unknown fields are rejected, including fields nested in a
locator or follow-up. Duplicate JSON object keys are a parse error. Arrays and text
fields required below must be non-empty.

## Top-level fields

| Field | Contract |
| --- | --- |
| `schema` | Exactly `mneme.review-ledger.v1`. |
| `artifact` | The frozen source review blob and the commit that contains it. |
| `response` | The counter-report blob and the commit that contains it. |
| `reviewed_commit` | The source cut the reviewer actually reviewed. It must resolve locally to a full commit object. |
| `source_findings` | The explicit source inventory, with unique IDs and unique artifact locators. |
| `entries` | Exactly one disposition for every source ID, and no entry for an undeclared ID. |

`artifact.artifact_commit`, `reviewed_commit`, and `response.response_commit` are
intentionally different fields with different jobs. The source report is read only
from `artifact_commit:path`; the reviewed cut is checked as a commit object; the
counter-report is read only from `response_commit:path`. Implementation evidence has
its own `evidence[].commit`. None is an implicit substitute for another. The verifier
does not require an ancestry relation or require the values to differ: imported
history and artifacts committed with their subject can both be legitimate. A ledger
author must not swap them merely because all are Git SHAs.

V1 binds exactly one immutable source report and one counter-report per ledger. A
review bundle containing heterogeneous reports uses one ledger per source report;
do not combine unrelated finding namespaces into one synthetic inventory.

`source_findings[].id` and `entries[].finding_id` use 1–128 ASCII letters, digits,
`.`, `_`, or `-`, starting with a letter or digit. A source ID and an entry ID must
form a complete one-to-one set. Source locator literals must also be unique, so two
IDs cannot silently point at the same artifact marker.

## Entry fields and dispositions

Every entry requires `finding_id`, `disposition`, `closure_tier`, `summary`,
`response_locator`, and a non-empty `evidence` array. Response locators must be unique
across entries and occur exactly once in the bound counter-report. `rationale` and
`follow_up` are the only optional fields; their presence is controlled by
disposition. A follow-up is exactly:

```json
{"owner":"named owner or project goal","action":"concrete next action or gate"}
```

The disposition enum is exact:

| Disposition | Meaning | `closure_tier` | `rationale` | `follow_up` |
| --- | --- | --- | --- | --- |
| `confirmed` | The concern is accepted and remains open. | `source_proof` | forbidden | required |
| `fixed` | The defect was fixed after the reviewed cut. | anything except `source_proof` | forbidden | forbidden |
| `already_fixed` | The reviewed claim is real but the named defect was already closed at the relevant implementation cut. | anything except `source_proof` | forbidden | forbidden |
| `invalid` | The finding is rejected by concrete source or reproduction evidence. | any tier actually evidenced | required | forbidden |
| `deferred` | The finding remains open behind a named future action or gate. | `source_proof` | required | required |
| `superseded` | A stronger contract replaces the proposed remedy. | any tier actually evidenced | required | forbidden |
| `not_applied` | A permanent non-fix or tradeoff was chosen intentionally. | any tier actually evidenced | required | forbidden |

`not_applied` is not a coy spelling of `deferred`: it records an intentional durable
decision and therefore has no pending follow-up. Adjacent limitations after a fixed
row should be stated in `summary` or opened as their own finding, not hidden in a
follow-up on a supposedly closed row.

A ledger with `confirmed` or `deferred` entries can be structurally valid. This
verifier establishes custody and complete disposition coverage; v1 has no
`--require-closed` mode and does not claim that every finding is closed.

## Evidence roles and closure tiers

The evidence `role` enum and the entry `closure_tier` enum use the same exact values:

- `source_proof`: committed implementation, reproduction, or source-analysis proof;
- `component_test`: a focused component/regression-test boundary;
- `installed_artifact`: evidence about the built or installed artifact actually run;
- `live_store`: evidence about an explicitly named live-store operation;
- `evaluation`: quality, scalability, comparator, or workload evidence.

These are claim categories, not a total order. Installed-artifact evidence does not
imply a live-store smoke, a live-store smoke does not imply evaluation quality, and
an evaluation does not imply packaging parity. `closure_tier` states the row's
declared claim boundary. Evidence must include a record with that exact role. Extra
roles are allowed and do not silently change the declared boundary.

Every disposition requires at least one `source_proof` record. In addition:

- `fixed` and `already_fixed` always require `component_test`, even when the declared
  tier is `installed_artifact`, `live_store`, or `evaluation`;
- a non-`source_proof` `superseded` row also requires `component_test`, so a runtime
  claim cannot rest only on prose about a stronger contract;
- `confirmed` and `deferred` remain declared at `source_proof`, though they may carry
  extra reproduction or runtime records;
- any declared tier requires an evidence record with that exact role.

An evidence record is exactly `role`, `path`, `commit`, `git_blob_sha256`, `locator`,
and `description`. Duplicate records with the same role, commit, path, and literal
are rejected.

## Git and locator binding

All commit fields are lowercase full object IDs: 40 hex characters in a SHA-1 Git
repository or 64 in a SHA-256 Git repository. The verifier asks the local repository
for its storage object format and rejects an abbreviation, a missing object, or an
object that is not a commit. It disables replacement objects and lazy fetching.

Embedded paths use `/`, are not absolute, contain no control characters or
backslashes, and contain no empty, `.` or `..` component. Blob bytes come directly
from `commit:path`; working-tree files are irrelevant.

`git_blob_sha256` is **not** Git's repository object ID and is not the ordinary
SHA-256 of file content. It is the SHA-256 of the canonical Git blob preimage:

```text
SHA256(b"blob " + decimal_byte_length + b"\0" + exact_blob_bytes)
```

This gives one portable SHA-256 custody digest even when the repository itself uses
SHA-1 object IDs. As a fixed vector, the empty blob hashes to
`473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813`;
the test suite also checks that value against a real SHA-256-format Git repository.

Each source-finding, response, and evidence locator is a non-whitespace UTF-8
`literal` plus
`literal_sha256 = SHA256(literal_utf8_bytes)`. The verifier checks the hash and then
requires the literal byte sequence to occur exactly once in the bound Git blob.
Overlapping occurrences count as multiple occurrences. Use a stable heading,
function signature, test name, or similarly specific marker—not a line number or a
one-word locator.

## What a valid result proves

A valid result proves only that:

- the ledger obeys this closed v1 shape and exact enums;
- the declared source inventory and entry IDs are unique and one-to-one;
- each entry has a unique exact-once counter-report locator;
- each source, response, and evidence commit/blob exists in the supplied local
  repository;
- each blob has the declared Git-object SHA-256;
- each declared literal has the declared locator hash and occurs exactly once;
- disposition-specific fields and required evidence-role records are present.

It does **not** prove that:

- the human-authored `source_findings` inventory captured every semantic finding in
  arbitrary prose, or that an ID was paired with the right heading;
- a response passage actually answers its source finding completely;
- a cited source passage actually proves the description or disposition;
- a named test was run, passed, covers the bug, or would fail without the fix;
- an installed artifact matched source, a live store was safe, or an evaluation was
  methodologically sound;
- the reviewed commit was the intended product cut, the artifact was independently
  authored, or the commits have a particular ancestry;
- no other defect exists.

Those are review judgments and execution/provenance claims. v1 makes omissions and
swapped bytes detectable; it does not launder a committed sentence into truth.

## Bounds and offline behavior

The verifier accepts ledgers up to 8 MiB, at most 4,096 source findings and entries,
at most 64 evidence records per entry and 4,096 total, locator literals up to 16 KiB,
at most 4 MiB of locator UTF-8 bytes in aggregate, and referenced blobs up to 64 MiB
each. The ledger is opened once with a nonblocking descriptor, required to be a
regular file, and read only through that descriptor up to the limit plus one byte.
This makes a path swap harmless and rejects FIFOs and devices without waiting for a
producer. A descriptor identity/size/timestamp change during the read is an
operational failure.

Git work has independent aggregate limits:

- references are batch-resolved and grouped by their resolved Git blob object ID, so
  identical bytes reached through different commits or paths are read only once;
- at most 256 MiB of unique blob bytes can be read in one invocation;
- exact-once checks can account for at most 512 MiB of locator-scan work, charged
  conservatively as two full-blob searches for every distinct locator in each blob;
- distinct per-blob locator needles can occupy at most 4 MiB;
- all Git batch control input is capped at 16 MiB;
- one verification can start at most 8 Git subprocesses, with a 30-second
  per-command deadline and a 90-second global Git deadline.

Blob contents are read through one local `cat-file --batch` process, verified one
resolved object at a time, and dropped before the next object. The verifier does not
retain one blob copy per evidence row. Aggregate blob or scan excess is a failed
ledger verification (`1`); command, deadline, or plumbing failure is operational
(`2`).

Every inherited `GIT_*` variable is removed before Git starts. The verifier then sets
only its fixed no-fetch, no-prompt, no-lock, no-system/global-config environment and
uses `--no-replace-objects`. It invokes only local Git plumbing (`rev-parse` and
`cat-file`). It never checks out a commit, runs a hook, executes an evidence command
or file, opens Mneme databases, starts a server, or performs a network fetch. Missing
objects fail closed; supply them locally before verification.

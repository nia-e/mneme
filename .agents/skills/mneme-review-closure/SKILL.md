---
name: mneme-review-closure
description: >-
  Triage or close an external, adversarial, or re-review finding set against Mneme
  with one-to-one dispositions, implementation evidence, sibling-surface attacks,
  and a counter-report. Use for read-only finding triage or, with repository-write
  authority, for saving feedback and fixing findings. Saving artifacts or editing
  code requires repository-write authority; creating commits requires separate
  explicit commit authority. Never infer either from a read-only review request.
---

# Close a Mneme review

Treat a review as an evidence ledger, not a vibes-based todo list. Preserve the
review unchanged, answer every item, and keep runtime claims narrower than the
evidence actually collected.

## 0. Choose the authority mode

Choose from the user's request and the authority actually available:

- **Read-only triage:** inspect the report, reviewed cut, code, and existing evidence;
  reproduce with non-mutating checks; return a complete proposed disposition map in
  the response. Do not save the report, copy templates, edit code, stage, or commit.
- **Write closure:** use only when the user asked to save, edit, fix, or otherwise
  produce repository artifacts and repository-write authority exists. Saving the
  frozen report, ledger, counter-report, implementation, and tests stays within that
  requested scope. Do not create commits yet.
- **Committed closure:** use only when explicit commit authority also exists. Commit
  authority is separate from permission to edit or fix. Only this mode may make the
  artifact, implementation, counter-report, and final-ledger commits described below.

If the requested closure needs a higher mode, stop at the strongest authorized
read-only or uncommitted result, state what remains provisional, and ask for the
missing authority. Never call a triage "closed" merely because its analysis is
complete.

## 1. Freeze the subject and source report

Work from a named branch with a clean understanding of any pre-existing edits.
Determine the exact full commit the reviewer inspected. Stop if that cut is
ambiguous; the current `HEAD` is not an automatic substitute.

In read-only triage, keep the source external and cite it without creating files. In
write closure, save the report verbatim at a stable repository path before editing
implementation. In committed closure, commit that artifact alone before editing
implementation. Record:

- the report path and artifact commit;
- the exact reviewed commit;
- any external provenance supplied by the reviewer;
- whether the report contains several independent finding namespaces.

Use one ledger per source report. Never silently normalize, renumber, or repair
the frozen report.

## 2. Inventory before fixing

Read `docs/review-ledger-schema.md` completely. In read-only triage, construct the
same inventory in the response without copying a template. In write closure, copy
`assets/review-ledger.template.json` outside the skill directory and enumerate every
semantic finding in `source_findings` before implementation begins. Use the reviewer's
stable IDs when unique; otherwise add a report-specific prefix without changing the
artifact. Bind each ID to a heading or other exact-once literal.

Create exactly one provisional entry per source finding. At this stage use only:

- `confirmed` when the defect reproduces and remains open;
- `deferred` when the concern is valid but belongs to a named future gate;
- `invalid` only when committed source or a concrete reproduction disproves it;
- `not_applied` only for an intentional permanent tradeoff, never for unfinished
  work.

Do not claim `fixed`, `already_fixed`, or `superseded` from design prose. Every
disposition needs committed source proof; runtime closure claims also need the
evidence tiers required by the schema.

## 3. Reproduce and partition work

For each finding, write the narrowest reproduction or source proof that can make
the claim falsifiable. Record adjacent surfaces with the same trust boundary.
Distinguish:

- implementation defects from documentation disagreement;
- current defects from already-fixed reviewed-cut claims;
- component behavior from installed-binary, live-store, and evaluation claims;
- root causes from duplicate symptoms.

Assign one accountable owner per mutable subsystem. Parallelize only disjoint
implementation slices or read-only audits. Give each owner the reviewed cut,
finding IDs, owned files, invariants, required tests, and explicit stop conditions.
One coordinator retains the canonical ledger and counter-report.

With commit authority, commit accepted fixes in small dependency-ordered rollback
points. Otherwise leave authorized edits uncommitted and mark commit-bound evidence
provisional. Do not rewrite or discard unrelated user changes.

## 4. Attack every proposed closure

Before changing a finding to a closed disposition, run both attacks:

1. **Claim-scope drift:** compare the exact source finding with the proposed
   response. Reject a response that fixes a narrower example while claiming the
   broader class.
2. **Sibling-surface attack:** inspect every unnamed operation that shares the
   relevant parser, capability, state transition, persistence layer, or public
   contract. Either test it, explain why the invariant excludes it, or open a new
   finding.

Also search for counter-evidence: older failures, crash states, stale artifacts,
capability loss, retries after lost acknowledgement, concurrency, malformed input,
and packaging/runtime skew. A stronger contract may justify `superseded`, but only
when committed behavior and component tests prove it.

## 5. Verify by evidence tier

In read-only triage, run only checks guaranteed not to modify the repository; list
mutation-producing tests as proposed evidence instead. Write closure may run the
scoped tests authorized by the user. Repository-write authority does not authorize
installing artifacts or mutating a live store; obtain the matching authority before
claiming those evidence tiers.

Run the smallest test that would fail without each fix, then the affected aggregate
suite. Add broader evidence only when the response claims it:

- `component_test`: focused regression plus affected suite;
- `installed_artifact`: build/install identity and finite smoke of that artifact;
- `live_store`: named backup/migration/store-health operation;
- `evaluation`: pinned workload, baselines, ablations, provenance, and artifact.

Record exact commands and results in the counter-report, but cite committed test or
source locators in the ledger. A passing workspace test does not prove an installed
binary or a live database. A flaky rerun is evidence of a flake, not permission to
erase the first failure.

## 6. Write the counter-report and bind evidence

In read-only triage, provide the proposed counter-report in the response. In write
closure, copy `assets/counter-report.template.md` outside the skill directory. Give
every source ID one exact, unique heading and answer it directly with:

- disposition and concise verdict;
- root cause or rejection rationale;
- exact implementation and test evidence;
- intentionally omitted work or residual limitation;
- follow-up owner/action for every open or deferred item.

Only in committed closure, commit all implementation, tests, and the counter-report
before filling their commit and Git-blob custody fields in the ledger. Without commit
authority, leave those fields provisional and do not claim custody-backed closure.
Then, for a committed ledger, run:

```sh
python3 tools/review_ledger_verify.py --repo . --ledger <ledger-path>
python3 tools/review_ledger_verify.py --repo . --ledger <ledger-path> --json
```

The verifier proves custody and coverage, not semantic truth. Independently reread
the source report, ledger, counter-report, and changed code after it passes. Confirm
that every source ID has one disposition, every response heading occurs once, every
evidence locator proves its description, and no uncommitted bytes are being cited.

With commit authority, commit the valid ledger as its own final review-closure
artifact. Otherwise report the uncommitted handoff and missing custody step. Report
remaining open/deferred/not-applied items explicitly; do not call the review “closed”
while any accepted finding lacks its promised evidence.

## Stop conditions

Stop and ask for direction when the reviewed commit cannot be established, the
source report cannot be preserved, a requested disposition would contradict the
evidence, required repository-write or commit authority is absent, or closure requires
live mutation/external coordination beyond the user's authority. Otherwise keep the
ledger honest and continue.

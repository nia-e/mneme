---
name: mneme-change-gates
description: >-
  Select, apply, and report the smallest explicit safety-gate set for Mneme
  changes spanning CLI or MCP interfaces, capability profiles, transports or
  packaging, persistent-store admission and leases, generation publication and
  recovery, or stale-writer compatibility. Use before planning, implementing,
  delegating, or reviewing one of these changes, including composites that cross
  several boundaries.
---

# Route a Mneme change through safety gates

Treat semantic operation classes as declarations of authority. Touched paths may
disprove or expose an incomplete declaration, but they never grant another class.
This skill covers the six trust boundaries below, not every Mneme change. Pure
retrieval/ranking, embedder, graph-policy, evaluation, privacy, or resource-control
work must use its own review workflow; do not invent a class merely to run this
selector.

## 1. Declare the change

Inspect the request and intended behavior before reading path hints. Declare every
class the change actually crosses:

| Class | Declare for |
| --- | --- |
| `public-interface` | CLI commands/modes or MCP tools, schemas, results, errors, and agent-facing recovery UX |
| `capability` | capability profiles, visibility, dispatch authority, receipts, or destructive/cold admission |
| `transport-package` | stdio/HTTP behavior, bounds, feature matrices, manifests, bundles, or installed binaries |
| `storage-admission` | persistent path resolution, classification, open/create policy, leases, release, or resume |
| `storage-publication` | detached construction, generation/selector publication, crash recovery, or cleanup |
| `stale-writer-compatibility` | format/catalog/generation/epoch fencing, old readers/writers, migration, or retry compatibility |

Repeat classes for a composite change. Do not collapse a composite into the class
that happens to own most files. Include every intended touched path, including
tests, manifests, docs, generated artifacts, and deleted files.

## 2. Run the selector

From the repository root, run:

```sh
python3 tools/change_gate_select.py \
  --class <class> [--class <class> ...] \
  --path <repo-relative-path> [--path <repo-relative-path> ...] \
  [--attest-path <repo-relative-path> '<why the declared classes cover it>']
```

Use `--json` when another tool or agent consumes the result. Do not rewrite or
post-process a failing result into success. Resolve every
`declaration_conflict`, `ambiguous_path_authority`, unknown class, and invalid
path, then rerun. An unclassified or multiplexed path produces
`needs-attestation` until its semantics have been inspected and a bounded rationale
is recorded with `--attest-path`. An attestation records judgment; it does not infer
or grant a class.

The selector is deliberately unable to infer intent from a filename. If intent is
still ambiguous after inspecting the task, stop and ask rather than selecting the
largest plausible pack on vibes.

`status: selected` means only that the declaration and path validation are closed.
It never means the gates or evidence have been satisfied.

## 3. Load only selected packs

Read each `selected_packs[].reference` completely. Do not load unselected packs.
Each selected gate ID is required until evidence shows it is genuinely not
applicable; record that rationale instead of silently omitting the ID.

Re-run the selector whenever the declared behavior or touched paths change. A
late edit can turn an admission-only change into a publication or compatibility
composite.

## 4. Attack sibling surfaces

For every selected pack, inspect unnamed siblings that share its parser, request
type, capability mint, transport adapter, store opener, lease, publication state
machine, generation marker, or retry proof. Probe at least:

- the corresponding CLI and MCP route when both exist;
- catalog/help visibility and dispatch, not only the successful handler;
- human/JSON and stdio/HTTP variants that share the operation;
- normal, release/resume, retry, and recovery paths;
- old/current/unknown persistent states when a format boundary is involved;
- packaged or installed artifacts when source behavior is claimed to ship.

An unnamed sibling may be excluded only by a written invariant or focused test.
Otherwise add its operation class and gates.

## 5. Report the gate result

Report these fields before calling the change ready:

1. declared classes and complete touched-path list;
2. selected packs and every required gate ID, each marked `satisfied`,
   `not_applicable` with rationale, or `missing`;
3. selector omissions plus justified unclassified paths;
4. sibling surfaces attacked and any new class they introduced;
5. selector `required_evidence_floor`, actual evidence tier, exact commands, and
   committed test/source locators;
6. downstream routing and residual limitations.

Evidence tiers are `source_proof`, `component_test`, `installed_artifact`,
`live_store`, and `evaluation`. The selector's tier is a floor, not proof. Do not
claim installed, live-store, or quality behavior from workspace tests.

Follow a selector route only when that skill/workflow exists. If a named future
route is unavailable, apply the selected pack directly and report that fact; do
not invent commands. Repository inventory or graph bootstrap routes to
`mneme-bootstrap`. Closure of an external finding set routes to
`mneme-review-closure` in addition to these change gates.

## Stop conditions

Stop when semantic intent remains ambiguous, selection is not `selected`, a required
gate lacks an honest disposition, or the needed installed/live mutation exceeds
current authority. Otherwise keep the declaration and evidence report current
through the final diff.

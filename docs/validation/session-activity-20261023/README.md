# Session activity validation

Implementation baseline: `995d4141923fb6d8015cd4b5e46f558463e45fea`. The original
working-tree snapshot is recorded in `source-files.json`; the subsequent human
output polish is recorded separately below. This is source and isolated artifact
validation, not a live integration rollout.

## Human output polish

The default view now says what the latest observation recorded, rather than
repeating "1 observation" and "latest receipt count" on every line. Separate
lanes, multi-observation counts, timestamp ambiguity, partial coverage and
unverified actor claims remain explicit. SAVE counts are not relabelled as notes
saved; they can include edition/episode identities. Structured JSON and detailed
event provenance are unchanged.

Follow-up checks passed:

- `cargo test --offline -p mneme-app -p mnemed -p mneme-mcp --no-default-features`:
  **745 passed**, including 41 reader/renderer tests; zero failures or skips.
- Default-feature CLI/MCP build and isolated installer/report smoke passed.
- Formatting and diff checks passed.

[Polished source hashes](polished-source-files.json),
[installed smoke](smoke-polished.json) and
[complete follow-up gate selection](polished-change-gates.json) identify this
snapshot. The API/CAP/DIST dispositions below still apply; the changed human
representation additionally has single/multiple observation, unknown-count,
save-count and unverified-checkpoint regressions. Earlier receipts are preserved,
not rewritten to claim the later formatting was tested in the earlier run.

## Original implementation results

Status: passed. Required evidence floor `installed_artifact` met by the isolated
installer/CLI/MCP smoke, not by a live rollout. Tested with Python 3.14.8
(`/opt/homebrew/bin/python3.14`) and Rust 1.100.0-nightly (`6bb1652a0`).

| Check | Result |
| --- | --- |
| Full Codex Python discovery | 1,977 run: 1,952 passed, 25 skipped |
| `mneme-app` aggregate | 180 passed, including 35 session-activity cases |
| `mnemed`, no default features, aggregate | 359 passed |
| `mneme-mcp`, no default features | 200 passed |
| `mneme-mcp`, no defaults plus HTTP | 234 passed |
| `mneme-mcp`, default features | 218 passed |
| `mnemed`, default unit/activity/init targets | 184 passed |
| Default build and isolated installer smoke | Passed |
| Formatting, diff, schema and documentation links | Passed |

Suites overlap; these numbers are not a unique-test total. Skipped native/platform
coverage is not claimed. Existing vendored/dead-code warnings remain outside this
change. Initial sandbox loopback-bind failures were rerun with approved local mock
access; no approval rejection or unresolved test failure remains.

[The smoke receipt](smoke-default.json) records executable/helper hashes, helper
inventory digest, CLI help/version and MCP identity. [Source hashes](source-files.json)
identify the working-tree source and operator documentation, excluding these
generated evaluation files to avoid self-referential hashes.
[Control-contract comparison](control-contracts.json) records the unchanged
constructors, validators, serializers and delivery attachment function.

## Observable boundary

`mnemed activity` and the dedicated stdio `session_activity` MCP server share one
bounded, local, read-only report. They do not discover/contact owners, open graph
stores, run inference, or repair/prune diagnostic files. The MCP instance pins its
admitted workspace and root at startup. All owner/router/library modes hide and
deny the new tool; existing graph `activity` semantics are unchanged.

The producer records only metadata from normal Codex work. It drains a bounded
diagnostic queue after functional selection budgets and transaction locks. Its
activity quota, lock and retention are separate from control state. Existing
reader/recording state constructors, validators and serializers remain unchanged;
byte-equality regressions cover the instrumented control paths.

Host admission is packet-level evidence from the existing recording observer,
not attribution to a particular selector attempt. Identical packets can arise
from different attempts, and missing best-effort receipts cannot establish
uniqueness. Admission therefore retains the recording job identity with unknown
selector generation/context, rather than reconstructing a link from journal
absence. Selection, consumption and final hook output retain their directly
observed producer provenance. Tied whole-second outcomes are reported as ambiguous
when they disagree, not ordered by receipt arrival.

## Reproduce

Use Python 3.11 or newer. Tests use disposable paths and mocks; local-loopback
fixtures require permission to bind a socket. No paid provider or live owner is
needed. Isolate the optional activity binding for the Python aggregate:

```sh
scratch=$(mktemp -d)
PYTHONDONTWRITEBYTECODE=1 MNEME_ACTIVITY_CONFIG="$scratch/missing.json" \
  python3 -m unittest discover -s integrations/codex -p 'test_*.py'
rmdir "$scratch"

cargo test --offline -p mneme-app
cargo test --offline -p mnemed --no-default-features
cargo test --offline -p mneme-mcp --no-default-features
cargo test --offline -p mneme-mcp --no-default-features --features http
cargo test --offline -p mneme-mcp
cargo test --offline -p mnemed --bin mnemed --test cli_activity --test cli_init

cargo build --offline -p mnemed -p mneme-mcp
python3 tools/session_activity_smoke.py \
  --cli target/debug/mnemed --mcp target/debug/mneme-mcp --features default \
  --receipt target/session-activity-smoke.json
```

The smoke uses the real installer in an isolated HOME/XDG tree, copies and checks
the supplied binaries/helper inventory, emits synthetic metadata through the
installed helper, compares CLI/MCP reports, exercises refusals and shutdown, and
checks that reporting did not change the fixture inventory. It never starts a
service, initializes a graph, trusts hooks or edits the operator's configuration.

## Regression locators

- [`mneme-app/session_activity/tests.rs`](../../../crates/mneme-app/src/session_activity/tests.rs):
  schema/privacy, workspace isolation, pinned scope/root, exact/prefix admission,
  incomplete coverage, atomic replacement, FIFO/symlink refusal, byte/output
  budgets, cancellation and ambiguous timestamp summaries.
- [`cli_activity.rs`](../../../crates/mnemed/tests/cli_activity.rs):
  CLI selector admission, human/JSON parity, no owner/discovery/subprocess contact
  and unchanged filesystem inventories.
- [`session_activity_surface_tests.rs`](../../../crates/mneme-mcp/src/session_activity_surface_tests.rs)
  and [`session_activity_stdio.rs`](../../../crates/mneme-mcp/tests/session_activity_stdio.rs):
  capability/mode matrix, strict tool arguments, transport refusal, framing,
  scope pinning and finite shutdown.
- [`test_session_activity.py`](../../../integrations/codex/test_session_activity.py):
  writer validation/privacy, contention, quota, stage recovery, retention,
  deferred queue bounds and diagnostic failure isolation.
- [`test_session_activity_hooks.py`](../../../integrations/codex/test_session_activity_hooks.py)
  and [`test_session_activity_workers.py`](../../../integrations/codex/test_session_activity_workers.py):
  quiet modes, core projection, selection/output distinctions, functional budgets,
  unlinked packet admission, save versus maintenance and unchanged control bytes.
- [`test_install_activity.py`](../../../integrations/codex/test_install_activity.py)
  and [`cli_init.rs`](../../../crates/mnemed/src/cli_init.rs):
  explicit setup authority, preserved bindings, drift/failure recovery, frozen old
  helper inventories and embedded/copied helper availability.

## Gate disposition

Declared classes: `public-interface`, `capability`, `transport-package`.
[The original unmodified selector result](change-gates.json) contains that
snapshot's touched paths and explicit attestations; the follow-up selection above
adds the polish evidence files. Both are `selected`, with no omissions or
pending attestations. Apply the selected packs directly; no downstream workflow
is selected. Independent diagnostic files do not change graph-store admission,
generation publication or existing control-ledger formats.

| Gate | Disposition and evidence |
| --- | --- |
| API-01 | satisfied — bounded shared request/schema admission and invalid-input tests |
| API-02 | satisfied — help/catalog versus raw-dispatch tests, including all sibling modes |
| API-03 | satisfied — shared projection and installed human/JSON/MCP parity smoke |
| API-04 | satisfied — explicit unknown/partial/unsupported outcomes, no legacy backfill or silent fallback |
| API-05 | satisfied — chooser/full/prefix happy paths, bounded details, installed help and guides |
| CAP-01 | satisfied — exhaustive owner-profile/configured/router/library/replica denial matrix |
| CAP-02 | satisfied — early selectors/dispatch denial with owner, discovery and subprocess tripwires |
| CAP-03 | satisfied — catalog/raw/stdio agreement and deliberate HTTP startup refusal |
| CAP-04 | satisfied — direct receipt semantics, packet-only host admission, save/maintenance/claim distinctions |
| CAP-05 | satisfied — pinned startup scope/root, generation/fence guards, no authority minted from historical receipts |
| DIST-01 | satisfied — stdio operation with explicit tested HTTP exclusion |
| DIST-02 | satisfied — pre-decode/frame/read/queue/output bounds and cancellation/overflow tests |
| DIST-03 | satisfied — no-default, HTTP and default-feature checks listed above |
| DIST-04 | satisfied — source inventory plus copied binary/helper/version/help identities in smoke receipt |
| DIST-05 | satisfied — finite initialize/catalog/report/refusal/EOF smoke and disposable-tree cleanup |

To repeat the exact semantic selection from the repository root:

```sh
python3 - <<'PY'
import json, subprocess
from pathlib import Path
gate = json.loads(Path('docs/validation/session-activity-20261023/change-gates.json').read_text())
args = ['python3', 'tools/change_gate_select.py']
for kind in gate['declared_classes']:
    args += ['--class', kind]
for path in gate['touched_paths']:
    args += ['--path', path]
for item in gate['path_attestations']:
    args += ['--attest-path', item['path'], item['rationale']]
subprocess.run(args + ['--json'], check=True)
PY
```

## Limitations

- Validation was on macOS. The reader is Unix-specific; non-Unix reporting fails
  closed. Linux and Windows native execution are not claimed by this receipt.
- Activity is prospective and best effort. Explicit CLI/MCP operations and other
  hosts remain untracked; no transcript or legacy-ledger backfill is performed.
- Thirty-day retirement is admission-time best effort, not a deletion deadline.
  Missing, malformed, pruned or byte-limited evidence remains unknown/partial.
- Host events without a native stable event ID cannot establish exact lifecycle
  retry identity. Recorded occurrences do not prove distinct starts/resumes.
- Normal prompt hooks and already-running workers can still run while an agent
  asks for a report. The report is not a conversation-wide pause mechanism.
- No real hook activation, service restart, live store change, remote deployment,
  provider workload or quality evaluation is included.

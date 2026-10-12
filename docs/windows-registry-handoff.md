# Windows + named-store registry status

**2026-10-12: local completion checks passed; ready for a native Windows trial.**
Nia resumed the work and will test on a Windows machine. Native Windows execution
is still missing; the passing macOS checks do not make this Windows-qualified.

## Request and boundaries

- Native Windows Codex hooks **and a local Windows database**, not merely WSL.
  Cross-platform Python was acceptable; a Rust-only rewrite was not required.
- Confirm/fix useful background tag maintenance beyond one tiny request.
- Implement a general named-store registry now: project store first, then
  configured stores with inline purposes, activation and explicit permissions.
  Global/misc are configuration defaults, not special architectural owners;
  company knowledge can be another entry. Remote paths belong to remote owners.
- Short wife-facing config migration guide after implementation; lower-priority
  TUI cluster naming/layout improvements.
- No production configuration/store migration, service rollout or publication
  occurred. Disposable test owners were stopped and cleaned up.

## Checkout and implementation

The implementation checkpoint was `dda36dd32f3e4f55f41512f1604747db8f039896`,
following `e318dd1`, with completion follow-ups in `d744776`: portable fixtures,
migration/smoke regressions, Windows CI test selection and documentation. This
snapshot is being landed on `main` without the separately retained migration
guide. No production runtime fixes were needed in the completion pass.

- Windows: Rust NTFS/ACL/identity/lease and body-publication support; native
  activation/snapshot boundaries; portable Python file/process/pipe helpers;
  installer, executable discovery and hook command handling.
- Late discovery repaired: vendored SQLite still rejected non-Unix existing
  sources. Vendor admission, closed evidence, backup custody and canonical drive
  URIs are now ported, with distinct Windows fingerprints and corresponding
  Cozo pins. Unix policy hashes were preserved. SQLite sidecar inheritance is
  checked at the actual parent; sidecar-only owner relaxation does not weaken
  main-file ownership. Native Windows behavior remains unverified.
- Registry: `integrations/codex/{store_registry,registry_config,registry_runtime,
  registry_reader,registry_recording}.py`, plus shared lifecycle/core/launcher,
  recording and stewardship integration. Frozen selection/purpose/owner fences,
  shared budgets, qualified identities, fair cursors and retained ledgers.
- Registry reads deliberately use `native_only` routing and warning-only
  concerns. Generic learned routing, routing witnesses and concern registration
  writes are **not implemented**; maintenance means guarded tag stewardship.
- Maintenance fixes remove idle-poll exhaustion of the content-work allowance,
  prevent cold-prefix starvation of hot candidates, and bound inference time.
- TUI naming/layout changes are present; the 246-test suite passed again and the
  earlier final render was reviewed. No additional redesign was attempted.

## Evidence already obtained

- Full Codex Python suite: **1,449 run, OK, 11 skipped** (Python 3.14), with the
  matching native CLI explicitly selected. Smoke harness: 11 passed.
- Rebuilt the matched model-independent `cozo,http` CLI/MCP pair under
  `target/windows-smoke-host/debug/` after feature-specific Cargo tests.
- Host vendor SQLite: 145 passed; Cozo opening: 28; admission: 29.
- Current CLI init/bundle tests: 5; executable/artifact contracts: 7; TUI: 246
  passed, 1 ignored. Prior init integration: 3; snapshot tests: 6.
- Windows GNU cross-target test compilation passed for the ported components.
  This is compilation, **not native Windows execution**.
- Disposable macOS installed two-owner registry smoke passed: project-first
  core, native owner-qualified reads, shared bytes and both-owner cleanup. The
  harness now rejects stale copied helpers or binaries, not merely records them.
  Final receipt `smoke-registry-unix-final-03.json`, SHA-256
  `3e7f4f2f0a7c829bc1e8c664e1148a86aba3b30b9c8e816afd9c97da0eacf8b1`.
- Migration: 19 focused tests and a disposable running-owner walkthrough passed.
  Detach, quiescence, prepare/apply and rollback preserved the PID, database
  identity, body and accounting. Receipt `migration-walkthrough-01.json`, SHA-256
  `ab862daaeb0bce86a0f02ed7e185b4b063adef1823ba82a185667cf1302980d4`.

Current machine-local receipts are under `docs/evaluation/windows-native-20261012/`
(gitignored), including exact commands, source/artifact hash inventory and
`GATE-REPORT.md`. All six classes and 34 gates have explicit dispositions; native
evidence gaps remain marked missing. Earlier raw evidence and the development
checkpoint remain under `docs/evaluation/windows-native-20261011/`. These are not
portable checked-in qualification artifacts.

## Windows trial and remaining boundaries

1. Run [the disposable Windows smoke](windows.md#try-it-without-touching-your-setup)
   against the matched provider-free build. No real store, Codex installation,
   provider credentials or administrator shell is required. Keep the new JSON
   receipt on either success or failure.
2. The workflow `.github/workflows/windows.yml` contains broader native ACL,
   SQLite/Cozo, process, publication and recovery suites. They have not run on
   Windows here. Neither has the default model runtime or real Codex hook delivery.
3. The separately retained migration walkthrough was checked on macOS, not
   executed in Windows PowerShell. It preserves existing owners and ledgers;
   it is **not** a WSL-to-Windows data migration. Offline Windows
   migration/reembedding remain unsupported.

Operator docs: [store separation](store-separation.md),
[registry design](proposals/named-store-registry.md),
[tag stewardship](tag-stewardship.md). Do not claim production Windows readiness
from the passing cross-builds or macOS smoke.

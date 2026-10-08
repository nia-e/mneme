# Transport and package gate pack

Apply this pack to stdio/HTTP adapters, framing and admission limits, Cargo features,
bundles, manifests, vendored inputs, release workflows, and installed executables.

## Required gates

- **DIST-01 — Transport parity:** Stdio and Streamable HTTP expose the intended same
  catalog, typed admission, capability denials, results, and error codes. Document
  deliberate transport-specific behavior.
- **DIST-02 — Pre-work bounds:** Bound encoded frames, decoded tool text, nesting,
  concurrent requests, inference/cold work, and cancellation before expensive work.
  A post-work response fuse is not an admission budget.
- **DIST-03 — Feature matrix:** Test relevant default/no-default/backend feature
  combinations. A binary missing the persistent backend must fail clearly rather
  than masquerading as a valid package.
- **DIST-04 — Artifact identity:** Bind bundle/manifest contents, source commit,
  features, version/help output, and installed binary hash. Source tests do not prove
  the binary found on `PATH` is current.
- **DIST-05 — Finite smoke and cleanup:** Exercise initialize, catalog, one bounded
  operation, refusal behavior, shutdown, and lease release with a timeout. Never put
  API keys or environment secrets in prompts, fixtures, logs, or manifests.

## Sibling attack

Check both transports, server and CLI packages, default and disabled features,
bundle metadata, system-wide installation targets, stale binaries, shutdown after
error, and held leases. A public shape change also selects `public-interface`; a
profile change also selects `capability`.

## Evidence

Minimum: `installed_artifact`. Record exact build/install commands, binary identity,
feature set, and finite smoke output. Use `tools/mcp_stdio_smoke.py` where applicable;
it is a smoke harness, not an arbitrary log parser or proof of live-store migration.

Apply these gates directly; no separate operations workflow is required. Before
installation or live mutation, confirm that the exact target, operation, and scope
are authorized. Record explicit binary and database paths, current source commit,
feature set, and source/bundle/installed-artifact hashes; do not infer the installed
binary from a passing source build. Back up and hash every affected live payload,
record the rollback path, and refuse ambiguous defaults or an unsupported command.
Run a finite smoke against an isolated install prefix and disposable stores first.
Only when live mutation is separately authorized, read back store health, identity,
and lease release after the transition. If authorization or any required evidence is
missing, stop that mutation and report the gap rather than treating a missing skill
as the blocker.

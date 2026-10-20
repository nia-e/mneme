# Stable native hook boundary: installed Codex proof

Status: **PASS** for the final symlink-selector implementation on the recorded
installed Codex 0.163.0 artifact. No live cutover performed.

## Question

Can native Codex trust remain unchanged across a Mneme runtime update, while an
already-loaded thread actually runs the new runtime?

The intended boundary is deliberately an **updatable installation**, not native
attestation of every Python dependency. The native command names one fixed real
shell entrypoint. Its selected executable wrapper binds the interpreter,
dispatcher and configuration for one runtime. Updating that selection should not
rewrite native recipes or native trust.

## Scope and isolation

The probe uses the installed daemon package as a **new isolated stdio process**,
not the running application daemon:

```text
/Users/nia/.codex/packages/app-server-daemon/releases/0.163.0-aarch64-apple-darwin/bin/codex
codex-cli 0.163.0
SHA-256 8813367285524ac798dd96d1b071aea30d58708c7226fd236cd275cf1cfdee31
```

Every run uses disposable private HOME, CODEX_HOME, XDG directories and cwd.
The child environment is constructed from an explicit whitelist; credentials and
proxy settings are not inherited. Effective `config/read` verifies file-only
auth storage, no MCP servers, disabled plugins/apps/memories and the fixture's
unauthenticated localhost provider before any thread starts. That provider only
returns an error and counts requests and accepted connections. Every exercised turn is blocked by the
synthetic hook before inference.

No production configuration, trust, app-server session, service, hook, store or
credential is changed. There are no Mneme store operations. Server requests are
refused rather than approved. RPCs have method, byte, frame and time bounds;
stderr is discarded, the local provider has socket timeouts, and each owned
process group is retired independently of its leader's exit status.

## Evidence

The [probe](probe_native_stable_hooks.py) imports the actual implementation's
`render_entrypoint` and `render_runtime`; it does not substitute a pretend native
hash or a separate shell renderer. Runtime programs are harmless synthetic
`hook_launcher.py` files which log a marker and return `decision: block`.

The script grants the native-provided hash exactly once before starting the
first thread. It compares native key/hash/trust, actual dispatch markers and turn
IDs, verifies unchanged native config/shim bytes through the stable-update phases,
then repeats catalog and dispatch in a fresh isolated app-server.

### Final symlink-selector proof

[Final receipt](native-0.163.0/receipt.json), using the frozen actual
`integrations/codex/stable_hooks.py` renderer:

```text
Base checkout: 8b54983474e20153b173f179c629c34127079b10
Helper SHA-256: d1f1a5dd742a8aa913d18c55e6820541b5682a18d5fb3d4ba473979d3c8fff31
Probe SHA-256: f1d6ed9d61780c38fa667a76b165d0491d2d5cfc7e0b8c003d7b876b281d1e6d
```

The helper and probe are working-tree additions at that base checkout; their
hashes, not the base commit alone, identify the tested source. Both hashes were
compared against the current files after the final run.

| Change | Native hash/trust | Actual dispatch |
| --- | --- | --- |
| Baseline | Trusted | `a1`, `config-a` |
| Selected runtime program bytes | Unchanged, trusted | Same thread: `a2`, `config-a` |
| Selector target | Unchanged, trusted | Same thread: `b1`, `config-a` |
| Runtime configuration bytes | Unchanged, trusted | Same thread: `b1`, `config-b` |
| Fresh isolated app-server | Unchanged, trusted | New thread: `b1`, `config-b` |
| Shim comment bytes, fresh server | Unchanged, trusted | Catalog observation only |
| Native command argument | Different, modified | Not dispatched |
| Native timeout definition | Different, modified | Not dispatched |
| Restore original native definition | Original, trusted | Catalog observation only |

There was exactly one native trust write and **zero provider connections and
requests**, including the final observation after server shutdown. The four
initial turns share one thread ID. Every logged event matches its returned native
turn ID. All six isolated app-server processes verified effective configuration
before use and retired their owned process groups.

Reproduce from the repository root, using a fresh output directory:

```sh
python3 docs/evaluation/stable-hooks-20261020/probe_native_stable_hooks.py \
  --codex /Users/nia/.codex/packages/app-server-daemon/releases/0.163.0-aarch64-apple-darwin/bin/codex \
  --helper integrations/codex/stable_hooks.py \
  --selector-kind symlink \
  --out target/stable-hooks-native-proof
```

The selector swap uses a new absolute symlink plus `os.replace`. The real shim
bytes, its native command, native configuration, and native trust remain unchanged
during the runtime-update phases. The receipt sanitizes temporary paths and the
local provider port; native hashes are recorded as observed, not reconstructed.

## Validation boundary

This proves only the recorded installed artifact and tested synchronous
`UserPromptSubmit` recipe. It does not enumerate Codex's complete hash algorithm,
prove behavior on a future Codex version, or exercise real Mneme retrieval,
recording, asynchronous workers, live cutover or persistent-store compatibility.
The shim-byte result is an observation of unchanged hash, not a claim that native
trust attests those bytes.

An unchanged catalog alone is insufficient. The same-thread logged runtime
changes are the evidence that the already-loaded recipe follows the selected
runtime without native reload or retrust.

Native trust is separate from Mneme's session/configuration admission fences.
This result does not bypass those fences, rebind existing lifecycle state, prove
paid work is admitted, or establish any live owner's health. It only removes the
native recipe/hash change from a runtime-only update in the tested design.

## Evidence-subtask gates

Declared classes: `public-interface`, `capability`, `transport-package`.
The [selector receipt](gates.json) names the complete evidence-subtask file list,
records every path attestation, and has no omissions. Its required evidence floor
is `installed_artifact`; this fixture meets that floor for the native behavior
above. Production implementation gates remain the containing change's
responsibility; this table does not claim that an evidence fixture validates
unrelated Mneme interfaces.

| Gate | Disposition for this evidence subtask |
| --- | --- |
| API-01 | `not_applicable`: no production request parser changed; fixture RPC allowlist and bounds are explicit. |
| API-02 | `not_applicable`: no Mneme catalog or dispatch changes; native catalog is compared with actual dispatch. |
| API-03 | `not_applicable`: no production CLI/MCP representation changed. |
| API-04 | `not_applicable`: no production API break or recovery contract changed. |
| API-05 | `satisfied`: reproducible command, deterministic receipt schema and explicit validation boundary. |
| CAP-01 | `not_applicable`: no Mneme operation/profile matrix changed. |
| CAP-02 | `not_applicable`: no production authority admission changed; no stores or inference are used. |
| CAP-03 | `not_applicable`: no production visibility or dispatch policy changed. |
| CAP-04 | `satisfied`: native trust, actual dispatch, session fences and paid/live authority are kept separate. |
| CAP-05 | `satisfied` within scope: same loaded thread and fresh process are both exercised; no claim about real Mneme workers. |
| DIST-01 | `not_applicable`: Codex native stdio is the selected host boundary; no Mneme stdio/HTTP parity change. |
| DIST-02 | `satisfied`: finite allowlisted RPC, bounded response bytes/frames/time, discarded stderr and no provider connections. |
| DIST-03 | `not_applicable`: no Cargo feature or backend artifact is built or changed. |
| DIST-04 | `satisfied`: explicit installed binary, version and SHA-256; exact probe/helper hashes and rendered shim recorded. |
| DIST-05 | `satisfied`: initialize, effective-config check, catalog, blocked dispatch, changed-definition refusal state, restart and owned-group cleanup. |

Sibling boundaries exercised: loaded versus fresh native recipes, runtime bytes
versus selector versus configuration, and command/timeout/shim-byte controls.
Other hook events, async execution, real memory delivery and live publication are
explicitly excluded rather than inferred from this synchronous fixture.

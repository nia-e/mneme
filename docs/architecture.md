# Architecture and development

Use this reference to find the code responsible for a change and choose its
checks. For installation and everyday commands, start with the
[README](../README.md). [AGENTS.md](../AGENTS.md) gives the project design criteria.

The table describes responsibility boundaries, not a requirement to create a
crate for every operation.

| Owner | Responsibility |
| --- | --- |
| `mneme-core` | Domain types, validation and ports; no database/model/transport I/O |
| `mneme-engine` | Retrieval, graph and learning policy |
| `mneme-app` | Shared save, context and other application workflows |
| `mneme-present` | Pure bounded response/context composition |
| `mneme-walk` | Constrained read-only traversal and receipt-facing state |
| `mneme-cozo`, `mneme-body`, `mneme-embed` | Persistence, body and inference adapters |
| `mneme-store-path` | Shared persistent-path/selector and lease infrastructure |
| `mneme-library`, `mneme-mcp-client` | Explicit multi-owner read coordination and remote client transport |
| `mnemed`, `mneme-mcp`, `mneme-tui` | CLI, MCP and interactive presentation |
| `integrations/codex`, `integrations/workshop` | Harness hooks, librarian orchestration and workshop lifecycle |
| `mneme-eval`, `mneme-vsa` | Evaluation and isolated projection research |

The key constraint is semantic ownership: frontends must not independently
reconstruct the same operation. A module extraction should remove a responsibility
from its old owner, not merely hide a dependency cycle in another file.

## Internal contracts and where to find them

Checked values establish local facts, not permanent authority. A valid request
does not prove that its owner is still selected, a lease is still held, or an
observed row is still current. Keep those checks at admission and commit.

| Contract | Owner |
| --- | --- |
| Provenance and capture replay proof | `mneme-core/src/provenance.rs` |
| Complete semantic/episode node construction and invariant-preserving mutation | `Node` in `mneme-core/src/lib.rs`, with episode rules in `episode.rs` |
| Snapshot wire formats and explicit predecessor codecs | `mneme-cozo/src/snapshot.rs`; backend callers retain strict-import versus repair-load policy |
| Catalog, generation marker and tag-policy combinations | Private `SchemaTarget` in `mneme-cozo/src/cozo_store/creation.rs` |
| Workspace evidence versus store-selection policy | `mneme-config/src/workspace.rs` and `selection.rs` |
| Retrieval policy identity and receipt-feedback row staging | `mneme-engine/src/retrieval_policy.rs` and `feedback.rs` |
| Context discovery and independent scene/touchstone composers | `mneme-app/src/context/`; `context.rs` owns the frozen window and orchestration |
| Card payloads and query envelopes | `mneme-present/src/cards.rs` and `query.rs`; the packer retains aggregate checks |
| Shared QUERY hit projection | `mneme-app/src/query.rs`; frontends still own body acquisition and budgets |
| Live versus snapshot catalog sources | `mneme-library/src/catalog_source.rs`; catalog metadata is not remote identity proof |
| Complete startup and admitted operation choices | MCP `startup.rs`, `capability.rs` and `main.rs`; CLI `remote_commands/request.rs` |

Construct a node in its final kind. Constructors and checked setters preserve
representational validity; adapters separately enforce immutable persisted
editions and transactional guards. Historical tag spellings remain representable
for explicit migration, without becoming valid new writes. Similarly, a capture
replay proof binds the original request bytes; it is not permission to replay
against any owner or epoch.

Prefer a small private enum when fields describe mutually exclusive states. Use a
shared generic only when actual consumers share its semantics: feedback's
`CachedRow<T>` pairs an original node/edge snapshot with its optional replacement,
but deliberately cannot create a missing row. There is no general typestate or
transaction framework around these operations.

## Backends and stored generations

Default native builds use the vendored **mnestic** fork of Cozo with SQLite/HNSW,
and fastembed with BGE-base embeddings. See the
[vendoring record](../vendor/mnestic/MNEME-VENDORING.md). No-default-feature
builds use a JSON-snapshot reference backend and lexical embedder; useful for
mechanism tests, not evidence of semantic retrieval quality.

Ordinary opens refuse supported predecessor generations rather than mutating or
guessing. Current stores use the `touchstones-v1` catalog with the canonical-tag
policy marker; current JSON snapshots use `mneme.store-export.v6`.
`single-graph-upgrade` builds detached successors through the named
`single-graph-v1`, `concern-v1`, `episode-context-v2` and `touchstones-v1` steps;
see the [migration contract](episodic-memory.md#existing-persistent-stores).
The separate [tag-format migration](tag-normalization.md) advances the tag policy
without rewriting historical submission proofs.
Inventory configured and retained stores before retiring migration support.
A current source format does not prove an existing store was migrated.

## Hot and cold paths

Retrieval composes embedding, sparse/dense candidates, optional graph expansion,
packing and optional reranking. Reads do not checkpoint, reinforce or create
co-retrieval edges, and traversal has no per-node model call. Typed coverage
separates retrieval partiality from final presentation omissions.

Whole-graph decay, pruning, community detection and reconciliation are explicit
maintenance work (the cold path), not side effects of retrieval. They require
`ColdPath` authority. The optional model-backed librarian
runs outside the native engine; its existence does not make native retrieval
model-generated, nor make the complete integration inference-free.

Resource accounting must include physical backend work and repeated calls, not
just returned nodes. The [memory-model reference](memory-model.md)
records public bounds; a task-wide context lease is not implemented.

## TODO: move shared hook machinery into Rust

Deferred, not implemented (2026-10-25). Python currently hosts more than harness
glue: startup/core composition, background readers, recording jobs and durable
accounting. Reduce the interpreter dependency by moving coherent responsibilities
behind the existing native application workflows, not by rewriting everything at
once or duplicating policy in a new frontend.

- **First:** a native passive startup/core hook entrypoint. Reuse `mneme-config`,
  `mneme-app`, `mneme-present` and owner clients; preserve bounded event admission,
  selected-owner scope, partial/unavailable coverage and nonfatal hook failures.
  Acceptance: ordinary startup/core delivery works without Python, with equivalent
  Codex/Claude adapter behavior and no inference, extra store writer or fallback.
- **Then:** migrate shared reader/recorder lifecycle and accounting as one owned
  responsibility, leaving host event/transcript codecs as thin adapters. Inventory
  current installations first; preserve job identities, replay receipts, pending
  work, packet budgets and uncertain-usage state. Replacing executable tooling
  must not reset or refill them. Validate interruption, restart and recovery before
  retiring the Python implementation or any required old-format readers.

Until then, default interpreter/tool selection should remain dynamic through
`PATH`, with explicit operator selectors supported. This TODO authorizes no live
deployment or state migration.

## Checks

Start with focused checks for the touched owners, for example:

```sh
cargo fmt --all -- --check
cargo test -p mneme-core
cargo test -p mneme-app
cargo run -p mnemed -- demo
```

Use `cargo test --workspace` for broad Rust integration coverage when warranted;
select relevant feature variants and Python integration suites for the changed
boundary. Read the [change-gates skill](../.agents/skills/mneme-change-gates/SKILL.md)
for CLI/MCP, capability, transport, storage or compatibility changes. Passing
workspace tests does not qualify an installed runtime or a live migration.

Workspace defaults do not enable the engine crate's own `cozo` test configuration.
For engine/adapter changes, also run `cargo test -p mneme-engine -p mneme-cozo
--features cozo`. Frontend changes may need both `--no-default-features` and
`--no-default-features --features cozo,http` on `mnemed` and `mneme-mcp`.

Synthetic mechanism checks and workload-quality evidence answer different
questions. No benchmark campaign is a default refactor gate.

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

## Backends and stored generations

Default native builds use the vendored **mnestic** fork of Cozo with SQLite/HNSW,
and fastembed with BGE-base embeddings. See the
[vendoring record](../vendor/mnestic/MNEME-VENDORING.md). No-default-feature
builds use a JSON-snapshot reference backend and lexical embedder; useful for
mechanism tests, not evidence of semantic retrieval quality.

Ordinary opens refuse supported predecessor generations rather than mutating or
guessing. Current stores use `touchstones-v1` (JSON v5).
`single-graph-upgrade` builds detached successors through the named
`single-graph-v1`, `concern-v1`, `episode-context-v2` and `touchstones-v1` steps;
see the [migration contract](episodic-memory.md#existing-persistent-stores).
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

Synthetic mechanism checks and workload-quality evidence answer different
questions. No benchmark campaign is a default refactor gate.

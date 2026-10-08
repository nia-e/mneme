# Mneme desktop extension (`.mcpb`)

Build a local desktop-extension bundle containing `mneme-mcp`. The extension
uses stdio and exposes the stores and capability profile selected at build time.
It includes the server binary, not database or memory-body contents.

## Build and install

Run from the Mneme checkout. You need Python 3, an installed `mneme-mcp`, and
Node/npm for the official packer (or `zip` as an offline fallback):

```sh
cargo install --locked --path crates/mneme-mcp
packaging/mcpb/build.sh \
  --capability-profile curator \
  --db project=/absolute/project/.mneme/memory.db \
  --model-cache /absolute/populated/.fastembed_cache \
  -o mneme-memory.mcpb
```

The default build includes the BGE-base-en-v1.5 f32 embedding model from an
existing fastembed cache. Use a cache populated by a matching Mneme runtime;
`--model-cache` selects it explicitly. The builder also searches conventional
local/data-directory caches. `--no-model` creates a smaller bundle but requires
network access for a first-use model download, so it is unsuitable for an offline
sandbox.

Install the resulting `.mcpb` in a compatible desktop extension client, such as
Claude Desktop's Extensions settings. The copied binary must match that machine's
OS/architecture. Review the generated manifest before sharing: it contains your
store paths and author, even though it does not contain memories.

## Use memory

Ask the desktop agent to recall a prior decision, inspect its source, or save a
verified lesson. Tool requests select a logical database name; for the example
above, use `db: "project"`. Writes require an explicit destination. Available
operations depend on the selected capability profile:

| Profile | Use |
| --- | --- |
| `read-only` | Recall and inspection. |
| `receipt-grounded` | Default; receipt-grounded learning without general authoring. |
| `curator` | Ordinary authoring, explicit links and advisory curation. |
| `operator` | Trusted maintenance, including destructive operations and lease control. |

Reserve `operator` for deliberate maintenance rather than a persistent desktop
agent. Profiles restrict tools; they do not authenticate individual callers.
See [CLI and MCP usage](../../docs/cli-and-mcp.md) for request examples.

The extension owns an exclusive lease on each configured store. Do not point it
at a store already owned by a Codex/HTTP host, or run another direct CLI/server on
it while the extension is active. To use an existing shared owner instead, connect
to its HTTP endpoint rather than packaging another owner.

## Build options

- Repeat `--db NAME=PATH` for multiple stores; use explicit absolute paths.
- `--bin PATH` or `MNEME_MCP_BIN` selects the copied server binary.
- `--author NAME` overrides the default Git author name.
- `-o FILE` selects the output file.
- `--model-cache DIR` or `MNEME_MODEL_CACHE` selects the model cache.

With no `--db`, the builder selects `user` at
`${XDG_DATA_HOME:-$HOME/.local/share}/mneme/memory.db` and `project` at
`$PWD/.mneme/memory.db`. Conventional `memory.db` selectors also resolve a valid
native `.mneme/current` generation; conflicting legacy files or invalid selectors
refuse startup. Use the exact intended store, not a nearby new database.

The builder prefers `@anthropic-ai/mcpb` (which validates the manifest), with
`zip` as a fallback. Run `packaging/mcpb/build.sh --help` for all options.

## Bundled embedding model (offline)

The model adds roughly 128 MB. The manifest sets `HF_HOME=${__dirname}/model` so
the runtime uses the bundled cache without downloading. Stores indexed with a
different embedder require a deliberate reindex/reembed; bundling a model does
not convert them.

## Why there's no `--http` in the bundle

The desktop client launches the server and communicates over stdin/stdout.
Adding `--http` would start a socket server that does not answer on that channel.
HTTP hosting is a separate setup; see [remote clients](../../docs/remote-cli.md).

## Updating

Rebuild and reinstall the bundle after updating `mneme-mcp`; the embedded copy
will not follow a later PATH upgrade. Storage upgrades, if needed, are separate
[offline operations](../../docs/episodic-memory.md#existing-persistent-stores).

# Mneme

Long-term memory for AI agents. Mneme stores notes and experiences in a searchable
graph, so agents can carry knowledge between sessions. It includes a CLI, an MCP
server, and a terminal graph browser.

## Install

Build from source with a recent Rust toolchain and native compiler tools. On
Linux, also install `pkg-config` and the OpenSSL development libraries.

```sh
git clone https://github.com/nia-e/mneme.git
cd mneme
cargo install --path crates/mnemed --locked
cargo install --path crates/mneme-mcp --locked
```

The default build uses local embeddings. Building can download ONNX Runtime;
the first save or semantic search downloads an embedding model unless cached.

## Set up a project

For **Codex**, install the Codex CLI and Python 3.11+, then run:

```sh
cd /path/to/your/project
mnemed init
```

This creates project memory, starts a local server, and enables automatic recall
and saving through Codex. Start a fresh Codex session afterward and follow any
hook-trust instructions from setup.

Automatic memory uses Codex models and can send session context to your configured
provider. Use `mnemed init --no-recording` to disable automatic saves while keeping
recall. See the [Codex guide](integrations/codex/README.md) for configuration.

For **other MCP clients or standalone CLI use**, follow the
[CLI and MCP guide](docs/cli-and-mcp.md).

## Use it

In a configured project:

```sh
mnemed save "Use the same operation ID when retrying a write."
mnemed query "retry after a timeout"
mnemed list
mnemed tui
```

You can add a longer note with `save --body`, inspect a result with `get NODE_ID`,
or browse connections in the TUI. Try `mnemed tui --demo` without setting up a store.

Use `mnemed <command> --help` for command options, or see the
[guides](docs/README.md) for tags, remote connections and other features.

## License

[MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE), at your option.
Vendored dependencies retain their own licenses.

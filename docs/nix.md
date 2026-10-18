# Nix client package

The flake currently packages **remote clients**, not a local memory server:

- `packages.<system>.client` (also `default`): `mnemed` and `mneme-mcp`, built
  together with `--no-default-features` from `Cargo.lock`;
- `apps.<system>.mnemed` (also `default`) and `apps.<system>.mneme-mcp`;
- `devShells.<system>.default`: Rust/Cargo, rustfmt, Clippy, pkg-config and Python;
- `checks.<system>.client-smoke`: finite installed-binary tests against a
  disposable synthetic loopback owner.

Outbound HTTP and SSH clients work without the local Cozo/SQLite backend,
FastEmbed/ONNX, model downloads, or the MCP HTTP **listener** feature. That last
omission does not disable outbound HTTP. Operations requiring the omitted
Cozo backend fail rather than creating an alternative persistent store.

This is a lightweight **feature profile, not a client-only authority sandbox**:
existing explicit offline lexical/JSON/reference and inspection paths remain.
Use configured remote owners for ordinary work; do not treat this package as a
production local owner. A full server package/service module
and ONNX/model-cache policy are deliberately deferred. This flake does not enable
services, enroll stores, install hooks, or enable background recording.

Outputs are declared for `x86_64-linux`, `aarch64-linux` and `aarch64-darwin`.
Only `x86_64-linux` has been built and smoke-tested. The two AArch64 outputs
remain unverified; an output being present is not build evidence.

## Build and develop

From a clean Git checkout containing the flake and lock file:

```sh
nix build .#client
./result/bin/mnemed --help
nix run .#mnemed -- --help
nix flake check
nix develop
cargo build --locked --no-default-features -p mnemed -p mneme-mcp
```

The development shell covers the remote-client feature set. It is not a promise
that default-feature ONNX/server builds work offline in a Nix sandbox.

### Private checkout boundary

**Do not use `path:/path/to/a/live/mneme-checkout` as a flake input.** Nix can copy
that entire directory into its world-readable store before the derivation's
source filter runs. A checkout can contain `.mneme/`, caches, local archives, or
credentials; filtering `src` later cannot undo that disclosure.

A normal Git flake includes tracked files, not arbitrary untracked files. Keep
private data untracked, review what is tracked, and use a pinned Git revision for
shared configuration. New untracked flake files must be tracked before Git flake
resolution can see them. For an unpublished draft, use a separately reviewed
source snapshot containing only the required source files and flake files; a
`path:` reference to **that sanitized snapshot** is safe to evaluate. Do not use a
raw-path override to work around untracked-file errors in the live checkout.

The package's additional source allowlist contains Cargo manifests/lock, licenses,
workspace crates, `vendor/mnestic`, integration sources (the CLI embeds its setup
bundle), and the bootstrap skill's embedded Python scripts. It excludes `.mneme`,
`.git`, build/model/Python caches, symlinks and unrelated root files. Tracked test
fixtures under `crates/` are source inputs, not live stores.

## Add it to a household configuration

Once a reviewed revision containing the flake is available, pin that revision in
the household flake lock. For example (replace the repository/revision with the
actual published source):

```nix
inputs.mneme.url = "github:OWNER/mneme/REVISION";
# Optional: use the household's already-reviewed package set.
inputs.mneme.inputs.nixpkgs.follows = "nixpkgs";
```

Install the package only for the intended user:

```nix
# Home Manager module with `inputs` in extraSpecialArgs.
{ inputs, pkgs, ... }: {
  home.packages = [ inputs.mneme.packages.${pkgs.stdenv.hostPlatform.system}.client ];
  xdg.configFile."mneme/memory.toml".text = ''
    schema = "mneme.config.v1"
    read_default = "personal"
    [background]
    enabled = false
    [stores.personal]
    id = "REPLACE_WITH_VERIFIED_OWNER_ID"
    owner = { url = "ssh://memory@example.test", db = "user", ssh_mcp_port = 19001 }
    active = { workspace = "any" }
    permissions = { read = true, write = false }
    delivery = { core = false, context = false }
    background = { recall = false, record = false, maintenance = false }
  '';
}
```

The example intentionally does **not** validate until the placeholder ID is
replaced. Use existing verified owner identities and connection parameters, not
copied database files. Configure user SSH keys/host verification separately; do
not put private keys, API tokens, or other secrets in Nix expressions or files
copied into the Nix store. The SSH client comes from the host's ordinary OpenSSH
installation. Another user's home profile should not inherit personal-memory
bindings.

Home Manager's symlinked `memory.toml` is supported. Only declarative TOML input
files follow symlinks; this does not relax database/body path policy. A configured
MCP process freezes and revalidates its selection: changing a link target or its
content requires restarting that process. See [declarative configuration](declarative-config.md).

Inspect the selection before connecting an agent:

```sh
mnemed --json config select --config "$HOME/.config/mneme/memory.toml" --cwd "$PWD"
mneme-mcp --memory-config "$HOME/.config/mneme/memory.toml"
```

Selection itself is metadata-only and does not contact owners. This package does
not move databases to the build machine or start another writer.

## Checks and artifact identity

The package builds only the two client binaries with default features disabled.
Its build phase runs focused, explicit no-default-feature tests for
`mneme-config`, `mneme-store-client`, CLI configuration routing, and configured
MCP. The separate installed-artifact smoke exercises symlinked configuration,
CLI/stdio MCP reads through an HTTP mock owner, identity forwarding, denied writes
before contact, catalog filtering, finite shutdown, and the disabled HTTP-listener
and omitted-Cozo boundaries. It uses no live owners, inference providers, model
cache or database leases. A client does not acquire a local owner lease, so no
live-store lease migration is implied by that smoke.

Inspect the exact package rather than whichever binary happens to be on `PATH`:

```sh
cat result/share/mneme/build-info.json
(cd result && sha256sum -c share/mneme/binary-sha256.txt)
```

The metadata binds the version, feature scope, Cargo lock hash, filtered source
store path and available Git/nixpkgs revision metadata. A raw sanitized snapshot
has no Git revision: `sourceRevision` is `null`, not a fabricated commit identity.
Keep its reviewed export hash separately. Binary hashes are written after Nix's
fixup/stripping phase. No system installation or running service is validated by
building an immutable package output.

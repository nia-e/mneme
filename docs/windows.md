# Native Windows

The native Windows port is **awaiting Windows runtime qualification**. The
filesystem and integration code cross-compile, and the ordinary packaged install
has passed a disposable macOS smoke test. Neither establishes that Windows ACLs,
file publication, hooks or recovery work on a real Windows host.

The [native Windows workflow](../.github/workflows/windows.yml) runs those checks
without provider credentials or production stores. You can try the same ordinary
installed smoke locally below; a native pass is still needed before treating this
port as a verified replacement. WSL and native Windows stores are separate
installations.

## Try it without touching your setup

From this checkout in native Windows PowerShell, with Rust's MSVC toolchain,
Visual Studio C++ build tools (including the Windows SDK), and Python 3.11+:

```powershell
cargo build --locked --no-default-features --features cozo,http -p mnemed -p mneme-mcp
if ($LASTEXITCODE -ne 0) { throw 'Build failed; do not run stale binaries' }
$receipt = Join-Path $PWD ('receipts/windows-smoke-' + [guid]::NewGuid().ToString('N') + '.json')
py -3 tools/windows_smoke.py --require-windows --cli target/debug/mnemed.exe --mcp target/debug/mneme-mcp.exe --timeout 300 --output $receipt --build-description 'cargo build --locked --no-default-features --features cozo,http -p mnemed -p mneme-mcp'
if ($LASTEXITCODE -ne 0) { throw "Smoke failed; keep the receipt at $receipt" }
```

Use `python` instead of `py -3` if the launcher is not installed; the interpreter
must be native Python 3.11+. Both the checkout and Windows temporary directory
must be on local NTFS. No Codex installation, provider credentials, administrator
shell or real store is needed for this smoke. It installs into fresh disposable
projects, tests two owners and their registry reads, and cleans up its processes
and stores. It does not install hooks into your existing projects.

Keep the JSON receipt, including on failure: it records artifact hashes, failed
stages and bounded subprocess diagnostics (labels, exit codes, byte counts and
output hashes), not raw memory bodies. Receipt paths are exclusive-create; rerun
with a new name rather than replacing evidence. A pass covers this finite
model-independent install path, not default model inference or real Codex hook
delivery. The fuller CI suite also creates symlink refusal fixtures; its fixture
privilege requirement is **not** a requirement to use Mneme or run this smoke.

## Intended ordinary setup

Use a recent Rust toolchain with the Visual Studio C++ build tools, native Codex,
Python 3.11 or later, and Windows PowerShell. Install both native executables from
the same checkout:

```powershell
cargo install --locked --path crates/mnemed
cargo install --locked --path crates/mneme-mcp
cd C:\Work\my-project
mnemed init
```

`init` discovers native `.exe` files and Python, including the Windows `py`
launcher. Explicit `--python`, `--codex-binary` and `--mcp-binary` paths remain
available. Shell-script wrappers such as `.cmd` are not native executables.

The integration still uses Python. This port changes the actual filesystem,
process and transport behavior; it is not a Rust launcher around Unix-only
scripts. Generated hooks work through an encoded PowerShell command so paths
are not interpreted differently by Codex's PowerShell and Command Prompt shells.
Review the generated hooks, then start a fresh Codex session.

Automatic recall and recording can send selected session material to your Codex
provider. `--no-recording` disables automatic saving, not model-backed recall.
`--no-trust-hooks` leaves hook trust pending manual review. See the
[Codex guide](../integrations/codex/README.md).

## Storage and recovery boundaries

- Local persistent storage is restricted to **local NTFS**. Network filesystems,
  UNC paths, reparse points and cloud-placeholder paths are not supported.
- Private files use Windows ACLs, not `chmod` as a privacy substitute. Existing
  unsafe files fail admission; reads do not repair their permissions.
- Owners hold an exclusive lease. A second process cannot open the same database
  by using a differently cased path. Use the existing owner for ordinary reads
  and writes.
- Managed body references are immediate filenames. A legacy absolute reference
  must identify an immediate child of the admitted body root; nested borrowed
  references remain Unix-only.
- Native Windows activation uses a versioned regular selector file. Unix uses
  its existing symlink selector. Copying a managed bootstrap between these
  formats is not a supported migration.
- Windows `migrate`, `reembed` / `reindex`, and `single-graph-upgrade` refuse before
  opening or changing storage. Their offline replacement/recovery workflows
  have not been ported. Preserve the matching database/body pair and use a
  verified Unix migration workflow where needed.
- Bootstrap's Python authoring workflow is not fully ported. Native creation and
  inspection are distinct from manifest-aware authoring and sidecar publication;
  do not infer the latter from a native storage test.
- Windows service stop terminates the verified process; it is not a Unix-style
  graceful signal/drain. Native tests must establish abrupt-exit recovery too.

Do not reuse an unsupported old hook ledger by deleting it or resetting spending.
The legacy Unix v1 hook-ledger upgrade remains Unix-only; native Windows refuses
it before creating a migration backup. Current ledgers retain usage and unknown
write outcomes across restarts.

## Configuration locations

Explicit `XDG_CONFIG_HOME` and `XDG_DATA_HOME` are honored on either platform.
Otherwise, new Windows configuration uses `%APPDATA%\mneme`, and device-local
data uses `%LOCALAPPDATA%\mneme`. `USERPROFILE\AppData\Roaming` and
`USERPROFILE\AppData\Local` are the fallbacks. These defaults do not move an
existing store or reinterpret a remote server's path as a local path.

## What the qualification checks establish

The workflow builds the published default feature set, then uses a separate
model-independent persistent build for finite tests of:

- private ACLs, reparse/hardlink refusal, lease contention and crash release;
- body and database publication, failure cuts, reopen and activation selectors;
- native pipes, bounded writes, process cleanup, and actual shell argv/stdin;
- `mnemed init` through its embedded installer and copied owner, followed by
  catalog, save/get/body, second-opener refusal, shutdown and reopen.

It does not establish model quality, real Codex app hook delivery, or the safety
of migrating a production store. Failed runs retain hashed evidence; a receipt
existing is not a passing result.

#!/usr/bin/env bash
# Pack the mneme-mcp *stdio* server into a .mcpb desktop-extension bundle.
#
# A .mcpb is launched by Claude Desktop and talks to it over stdio — so this
# bundles an explicit capability profile plus the --db flags. Do NOT add --http
# here (see README.md).
#
# By default it also bundles the fastembed embedding model so the extension
# works offline (a sandbox can't download it). HF_HOME points the server at the
# bundled copy; hf-hub serves cached files without touching the network.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bin="${MNEME_MCP_BIN:-}"
out="mneme-memory.mcpb"
author="${MCPB_AUTHOR:-}"
model_cache="${MNEME_MODEL_CACHE:-}"
with_model=1
capability_profile="receipt-grounded"
dbs=()

# The HF repo fastembed pulls for EmbeddingModel::BGEBaseENV15 (DEFAULT_DIM=768).
REPO="models--Xenova--bge-base-en-v1.5"

usage() {
  cat <<'EOF'
Usage: build.sh [--db NAME=PATH ...] [--capability-profile PROFILE]
                [--bin PATH] [-o OUT] [--author NAME]
                [--model-cache DIR] [--no-model]

  --db NAME=PATH    a database to expose, by logical name (repeatable). Default:
                      user=$XDG_DATA_HOME/mneme/memory.db
                      project=$PWD/.mneme/memory.db
  --capability-profile PROFILE
                    read-only, receipt-grounded, curator, or operator
                      (default: receipt-grounded; operator is intentionally never
                      selected implicitly)
  --bin PATH        the mneme-mcp binary to embed
                      (default: $MNEME_MCP_BIN, else `mneme-mcp` on PATH)
  -o OUT            output path (default: ./mneme-memory.mcpb)
  --author NAME     manifest author (default: `git config user.name`)
  --model-cache DIR a `.fastembed_cache` directory holding the embedding model
                      (default: auto-detected; or $MNEME_MODEL_CACHE)
  --no-model        don't bundle the model (smaller, but needs network on first
                      use — not usable in an offline sandbox)

Example (explicit user and project stores):
  build.sh --capability-profile curator \
           --db user=$HOME/.local/share/mneme/memory.db \
           --db project=$PWD/.mneme/memory.db \
           -o ~/Downloads/mneme-memory.mcpb
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --db) dbs+=("$2"); shift 2 ;;
    --capability-profile) capability_profile="$2"; shift 2 ;;
    --bin) bin="$2"; shift 2 ;;
    -o | --out) out="$2"; shift 2 ;;
    --author) author="$2"; shift 2 ;;
    --model-cache) model_cache="$2"; shift 2 ;;
    --no-model) with_model=0; shift ;;
    -h | --help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 1 ;;
  esac
done

case "$capability_profile" in
  read-only | receipt-grounded | curator | operator) ;;
  *)
    echo "error: invalid capability profile: $capability_profile" >&2
    echo "       expected read-only, receipt-grounded, curator, or operator" >&2
    exit 1
    ;;
esac

[[ -n "$bin" ]] || bin="$(command -v mneme-mcp || true)"
[[ -x "$bin" ]] || {
  echo "error: mneme-mcp binary not found — pass --bin PATH, set MNEME_MCP_BIN," >&2
  echo "       or run: cargo install --path crates/mneme-mcp" >&2
  exit 1
}

if [[ ${#dbs[@]} -eq 0 ]]; then
  dbs=("user=${XDG_DATA_HOME:-$HOME/.local/share}/mneme/memory.db" "project=$PWD/.mneme/memory.db")
fi
[[ -n "$author" ]] || author="$(git config user.name 2>/dev/null || echo mneme)"

# Locate the model cache unless --no-model.
if [[ "$with_model" == 1 && -z "$model_cache" ]]; then
  for d in "./.fastembed_cache" "${XDG_DATA_HOME:-$HOME/.local/share}/mneme/.fastembed_cache" "$HOME/.fastembed_cache"; do
    if [[ -f "$d/$REPO/refs/main" ]]; then model_cache="$d"; break; fi
  done
fi
if [[ "$with_model" == 1 && ( -z "$model_cache" || ! -f "$model_cache/$REPO/refs/main" ) ]]; then
  echo "error: embedding model not found in any .fastembed_cache." >&2
  echo "       Populate it once (e.g. \`mnemed --user query x\`), pass --model-cache DIR," >&2
  echo "       or pass --no-model to build without it (needs network at run time)." >&2
  exit 1
fi

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/server"
cp "$bin" "$stage/server/mneme-mcp"
chmod +x "$stage/server/mneme-mcp"

# Bundle the model: refs/ + snapshots/ only (dereferenced into real files), no
# blobs/ dup and no .lock files — hf-hub's cache-first lookup reads refs ->
# snapshots/<commit>/<file> and never touches blobs on a hit.
if [[ "$with_model" == 1 ]]; then
  dst="$stage/model/$REPO"
  mkdir -p "$dst"
  cp -RL "$model_cache/$REPO/refs" "$dst/refs"
  cp -RL "$model_cache/$REPO/snapshots" "$dst/snapshots"
  find "$stage/model" -name '*.lock' -delete
fi

# Render the manifest: inject the capability profile, --db args, the author, and
# (when bundled) HF_HOME.
python3 - "$here/manifest.template.json" "$stage/manifest.json" "$author" "$with_model" "$capability_profile" "${dbs[@]}" <<'PY'
import json, sys
template, out_path, author, with_model, capability_profile, *dbs = sys.argv[1:]
m = json.load(open(template))
m["author"]["name"] = author
m["server"]["mcp_config"]["args"] = [
    "--capability-profile",
    capability_profile,
    *[x for spec in dbs for x in ("--db", spec)],
]
if with_model == "1":
    m["server"]["mcp_config"]["env"] = {"HF_HOME": "${__dirname}/model"}
json.dump(m, open(out_path, "w"), indent=2)
PY

abs_out="$(python3 -c 'import os,sys; print(os.path.abspath(sys.argv[1]))' "$out")"

# Prefer the official packer (it validates the manifest); a .mcpb is just a zip,
# so fall back to `zip` when offline.
if npx -y @anthropic-ai/mcpb pack "$stage" "$abs_out"; then
  :
else
  echo "note: @anthropic-ai/mcpb unavailable — packing with zip instead" >&2
  (cd "$stage" && zip -qr -X "$abs_out" .)
  echo "Output: $abs_out"
fi

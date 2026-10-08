#!/usr/bin/env python3
"""Finite, device-local project setup. No publishing/downloads; exact shipped hook trust through native Codex RPCs."""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import sys
import uuid
import tomllib

import install
from hook_trust import trust_project_hooks
from profile import load_profile
from misc_binding import _codex_configured
from service import ServiceConfig, ensure_ready, load_config, _expected_catalog
from mcp_client import McpClient

SCHEMA = "mneme.project-init.v1"
DB_ID = re.compile(r"[0-7][0-9A-HJKMNP-TV-Z]{25}\Z")


def read_json(path, limit=1024 * 1024):
    raw = install.read_optional(Path(path))
    if raw is None:
        return None
    if len(raw) > limit:
        raise ValueError(f"configuration exceeds limit: {path}")
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError(f"duplicate configuration field in {path}")
            result[key] = value
        return result
    return json.loads(raw, object_pairs_hook=pairs)


def executable(value, name, sibling=None):
    source = value or (sibling if sibling and sibling.is_file() else shutil.which(name))
    if source is None:
        raise ValueError(f"{name} executable unavailable; install it explicitly or pass --{name if name != 'mneme-mcp' else 'mcp'}-binary PATH (no builds/downloads performed)")
    source = Path(source).resolve(strict=True)
    if not source.is_file() or not os.access(source, os.X_OK):
        raise ValueError(f"{name} executable is not usable")
    return source


def native(mnemed, root, *args):
    # Native init/status are bounded and offline only when no configured owner exists.
    result = subprocess.run([str(mnemed), "--json", *map(str, args)], cwd=root,
                            stdin=subprocess.DEVNULL, capture_output=True, timeout=40)
    if len(result.stdout) > 512 * 1024 or len(result.stderr) > 64 * 1024:
        raise ValueError("native setup response exceeds its bound")
    if result.returncode:
        raise ValueError(result.stderr.decode(errors="replace")[:2048].strip() or "native setup refused")
    return json.loads(result.stdout)


def registry_path(selection, explicit):
    chosen = selection["library_config"]
    if explicit and chosen and Path(explicit).resolve() != chosen.resolve():
        raise ValueError("--library-config conflicts with the project's explicit library selection")
    if explicit or chosen:
        return Path(explicit or chosen).absolute()
    data = os.environ.get("XDG_DATA_HOME")
    if not data:
        home = os.environ.get("HOME")
        if not home:
            raise ValueError("cannot locate device-local registry; set HOME/XDG_DATA_HOME or --library-config")
        data = str(Path(home) / ".local/share")
    path = Path(data)
    if not path.is_absolute():
        raise ValueError("XDG_DATA_HOME must be absolute")
    return path / "mneme/libraries/personal/library.json"


def configured_integration(root):
    """Adopt only the existing explicit shipped launcher, never guess an owner."""
    raw = install.read_optional(root / ".codex/config.toml")
    config = {} if raw is None else tomllib.loads(raw.decode())
    server = config.get("mcp_servers", {}).get(install.SERVER)
    if server is None:
        return None
    if not isinstance(server, dict) or server.get("enabled", True) is not True:
        raise ValueError("existing project Mneme integration is explicitly disabled; init will not enable it")
    args = server.get("args")
    if (not isinstance(args, list) or len(args) != 3 or not all(isinstance(x, str) for x in args)
            or Path(args[0]).name != "launcher.py" or args[1] != "--service-config"
            or not Path(args[2]).is_absolute()):
        raise ValueError("existing Mneme integration is not the shipped local launcher; preserve it and configure its owner explicitly")
    service_path = Path(args[2])
    hook_path = service_path.parent / "hooks.json"
    # Validate the existing hook schema through its existing reader, not another schema.
    from hooks import _config
    hook = _config(hook_path)
    if hook["project_root"] != root or Path(hook.get("service_config", "")) != service_path:
        raise ValueError("existing integration belongs to another project/service")
    return service_path, hook_path, hook


def catalog(config):
    with McpClient(config.url, token_env=config.token_env or None, timeout=3) as client:
        value = client.call_tool("databases", {})
    if not _expected_catalog(config, value):
        raise ValueError("owner catalog does not match the exact project path/alias")
    db_id = value[0].get("db_id")
    if not isinstance(db_id, str) or not DB_ID.fullmatch(db_id):
        raise ValueError("owner catalog has no canonical database identity")
    return db_id


def import_library(path):
    spec = importlib.util.spec_from_file_location("mneme_setup_library", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def setup(args, result):
    requested = Path(args.root).absolute()
    if requested.is_symlink() or not requested.is_dir():
        raise ValueError("project root must be an existing real directory")
    root = requested.resolve(strict=True)
    result["project_root"] = str(root)
    dot = root / ".mneme"
    for path in (dot, root / ".codex"):
        if path.is_symlink() or (path.exists() and not path.is_dir()):
            raise ValueError(f"setup requires a real directory or absent path: {path}")
    # Preserve exclusions on both lexical and canonical ancestry. Setup is not
    # an escape hatch from an inherited private/isolated project.
    seen = set()
    for chain in (requested, root):
        for index, parent in enumerate((chain, *chain.parents)):
            if index >= 64:
                raise ValueError("project setup ancestry exceeds its bound")
            if parent in seen:
                continue
            seen.add(parent)
            inherited = load_profile(parent)
            if inherited["mode"] != "default":
                raise ValueError(f"project profile is explicitly {inherited['mode']}; init preserves that choice")
            if parent != root and ((parent / ".mneme").exists() or (parent / ".mneme").is_symlink()
                    or _codex_configured(parent / ".codex")):
                raise ValueError("project inherits an existing Mneme boundary; initialize its actual root instead")
    selection = load_profile(root)
    if selection["mode"] != "default":
        raise ValueError(f"project profile is explicitly {selection['mode']}; init preserves that choice")
    registry = registry_path(selection, args.library_config)
    # Do not create a parallel conventional database beside an activated/other store.
    for name in ("memory.db", "current", "generations"):
        path = dot / name
        if path.exists() or path.is_symlink():
            raise ValueError(f"ambiguous project storage at {path}; adopt/configure that existing store explicitly, not a second database")
    if dot.exists():
        entries = []
        for index, path in enumerate(dot.iterdir()):
            if index >= 4096:
                raise ValueError("project storage directory exceeds setup inspection bound")
            entries.append(path)
        if any(path.name != "codex-memory.db" and path.suffix.lower() in (".db", ".sqlite", ".sqlite3")
               for path in entries):
            raise ValueError("ambiguous custom project storage; configure/adopt the existing store explicitly")
    db = dot / "codex-memory.db"
    if db.is_symlink() or (db.exists() and (not db.is_file() or db.stat().st_nlink != 1)):
        raise ValueError("project store must be a single-link real file")
    mnemed = executable(args.mnemed, "mnemed")
    # This overrides only this short-lived setup's native transport adapter.
    os.environ["MNEME_CLIENT_BINARY"] = str(mnemed)
    toml_raw = install.read_optional(root / ".codex/config.toml")
    project_toml = {} if toml_raw is None else tomllib.loads(toml_raw.decode())
    toml_hooks = project_toml.get("hooks", {})
    if not isinstance(toml_hooks, dict) or toml_hooks.get("enabled", True) is not True:
        raise ValueError("project TOML hooks are explicitly disabled or malformed; init will not enable them")
    hooks_doc = read_json(root / ".codex/hooks.json")
    if hooks_doc is not None:
        if not isinstance(hooks_doc, dict) or hooks_doc.get("enabled", True) is not True:
            raise ValueError("project hooks are explicitly disabled or malformed; init will not enable them")
        if hooks_doc.get("hooks", {}).get("enabled", True) is not True:
            raise ValueError("project hooks are explicitly disabled; init will not enable them")
    existing = configured_integration(root)
    prefix = dot / "codex-integration"
    if existing is None and prefix.exists():
        pending = prefix / "pending.json"
        if pending.is_file():
            result["stage"] = "installer_recovery"
            recovered = install.recover(pending)
            if recovered["status"] == "aborted":
                prefix.rename(dot / ("codex-integration-aborted-" + uuid.uuid4().hex))
            existing = configured_integration(root)
        if existing is None and prefix.exists():
            raise ValueError(f"unclaimed integration prefix {prefix}; preserve it and review pending.json/receipt.json before retrying")
    expected_db_id = None
    previous_owner = read_json(dot / "cli.json", 8192)
    if existing is None:
        if _codex_configured(root / ".codex"):
            raise ValueError("other existing Mneme/managed project integration; init preserves it instead of installing a second one")
        if previous_owner is not None:
            raise ValueError("project CLI owner already exists without a matching local Codex integration; init will not replace or compete with it")
        mcp = executable(args.mcp_binary, "mneme-mcp", mnemed.parent / "mneme-mcp")
        codex = executable(args.codex_binary, "codex")
        if args.port is None:
            with socket.socket() as listener:
                listener.bind(("127.0.0.1", 0))
                port = listener.getsockname()[1]
        else:
            port = args.port
        # All configuration/precondition validation precedes storage creation.
        plan = install.prepare(root, prefix, mnemed, mcp, port, recall_mode="async",
                               recording_mode="off" if args.no_recording else "automatic",
                               reader_model=install.READER_MODELS[0], reader_codex=codex)
        result["stage"] = "store_admission"
        dot.mkdir(mode=0o700, exist_ok=True)
        if db.exists():
            verified = native(mnemed, root, "--db", db, "capture", "inspect")
            expected_db_id = verified.get("db_id")
            if not isinstance(expected_db_id, str) or not DB_ID.fullmatch(expected_db_id):
                raise ValueError("native store inspection supplied no canonical identity")
            result["store"] = "adopted"
        else:
            native(mnemed, root, "--db", db, "capture", "init")
            verified = native(mnemed, root, "--db", db, "capture", "inspect")
            expected_db_id = verified.get("db_id")
            if not isinstance(expected_db_id, str) or not DB_ID.fullmatch(expected_db_id):
                raise ValueError("created store inspection supplied no canonical identity")
            result["store"] = "created"
        result["stage"] = "integration_install"
        applied = install.apply(plan)
        result["receipt"] = applied["receipt"]
        existing = configured_integration(root)
    else:
        result["store"] = "adopted"
    service_path, hook_path, hook = existing
    config = load_config(service_path)
    if (not isinstance(config, ServiceConfig) or config.database_name != "project"
            or config.database_path != db or config.working_directory != root):
        raise ValueError("existing service is not the exact local project owner; no alternate owner started")
    if args.port is not None and args.port != config.port:
        raise ValueError("--port conflicts with existing owner; init does not reroute configured services")
    if previous_owner is not None:
        if (not isinstance(previous_owner, dict)
                or set(previous_owner) - {"schema", "url", "database", "db_id", "token_env"}
                or not isinstance(previous_owner.get("db_id"), str)
                or not DB_ID.fullmatch(previous_owner["db_id"])
                or previous_owner.get("schema") != "mneme.cli.owner.v1"
                or previous_owner.get("url") != config.url
                or previous_owner.get("database") != config.database_name):
            raise ValueError("existing CLI owner route conflicts; no alternate owner started")
    enrollment = read_json(service_path.parent / "enrollment.json", 8192)
    if enrollment is not None and enrollment.get("mode") not in (None, "none"):
        raise ValueError("existing integration has publishing enrollment; review/disable it explicitly before device-local init")
    result.update(service_config=str(service_path), owner_url=config.url)
    result["stage"] = "owner_start"
    ensure_ready(config, timeout=10)
    db_id = catalog(config)
    if expected_db_id is not None and expected_db_id != db_id:
        raise ValueError("project database identity changed between adoption and owner startup; no CLI binding written")
    result["db_id"] = db_id
    owner = {"schema": "mneme.cli.owner.v1", "url": config.url,
             "database": config.database_name, "db_id": db_id}
    if config.token_env:
        owner["token_env"] = config.token_env
    if previous_owner is not None and previous_owner != owner:
        raise ValueError("existing CLI owner identity/route conflicts; init will not replace it")
    # Explicit recording-off changes only the existing recorder choice. Ordinary
    # reruns never upgrade an existing off/reminder/shadow configuration.
    if args.no_recording and hook.get("recording_mode") == "automatic":
        raw_hook = read_json(hook_path)
        raw_hook["recording_mode"] = "off"
        install.private_write(hook_path, install.json_bytes(raw_hook))
        hook["recording_mode"] = "off"
    result["recording"] = hook.get("recording_mode", "off")
    result["hippocampus"] = hook.get("memory_mode", hook.get("recall_mode", "reminder"))
    result["stage"] = "local_registration"
    registered = import_library(args.library_helper).register_local_project(
        registry, root, db_id, database=config.database_name,
        owner_endpoint={key: owner[key] for key in ("url", "token_env") if key in owner})
    if registered and selection["library_config"] is None:
        install.private_write(dot / "profile.json", install.json_bytes({"schema": "mneme.profile.v1",
            "mode": "default", "library_config": str(registry)}))
    if previous_owner is None:
        install.private_write(dot / "cli.json", install.json_bytes(owner))
    result.update(status="configured", stage="complete", local_registration=registered,
                  library_config=str(registry), hook_trust="unchanged; init does not grant Codex hook trust",
                  recording_activation="requires Codex hook trust; review project hooks in Codex",
                  published=False, sharing="unchanged; no enrollment/publication performed")
    if args.no_trust_hooks:
        result["hook_trust"] = {"status":"not_requested", "trusted":0,
            "reason":"explicit --no-trust-hooks; review installed project hooks in Codex"}
    else:
        try:
            codex = executable(args.codex_binary or hook.get("reader_codex") or hook.get("shadow_codex"), "codex")
            result["hook_trust"] = trust_project_hooks(codex, root, service_path)
        except Exception:
            result["hook_trust"] = {"status":"pending", "trusted":0, "reason":"codex_unavailable"}
    result["recording_activation"] = ("enabled hook definitions trusted; fresh-session execution not yet verified"
        if result["hook_trust"]["status"] == "trusted" else "hook trust pending; review project hooks in Codex")



def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("root", "mnemed", "library-helper"):
        parser.add_argument("--" + name, required=True, type=Path)
    for name in ("mcp-binary", "codex-binary", "library-config"):
        parser.add_argument("--" + name, type=Path)
    parser.add_argument("--port", type=int)
    parser.add_argument("--no-recording", action="store_true")
    parser.add_argument("--no-trust-hooks", action="store_true")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    result = {"schema": SCHEMA, "status": "incomplete", "stage": "preflight", "published": False}
    try:
        setup(args, result)
    except Exception as error:
        result["error"] = str(error)[:2048]
        result["recovery"] = "Preserved data/configuration; resolve the reported conflict, then rerun mnemed init. An installer pending.json is recovered on rerun; do not delete memory data."
    if args.json:
        print(json.dumps(result, ensure_ascii=False))
    elif result["status"] == "configured":
        print(f"Project memory {result['store']}: {result['project_root']}\nOwner {result['db_id']} ready; {('locally registered' if result['local_registration'] else 'not registered: existing exclusion/withdrawal preserved')} (no publishing performed).\nHippocampus: {result['hippocampus']}; recording: {result['recording']}.\nHook trust: {result['hook_trust']['status']} ({result['hook_trust']['trusted']} trusted enabled hooks). Fresh-session execution is not verified.")
        if result["hook_trust"]["status"] != "trusted":
            print(f"Hook review remains manual ({result['hook_trust'].get('reason', 'pending')}); use Codex /hooks. Project store/owner remain configured.")
    else:
        print(f"Project init incomplete at {result['stage']}: {result['error']}\n{result['recovery']}", file=sys.stderr)
    return 0 if result["status"] == "configured" else 1


if __name__ == "__main__":
    raise SystemExit(main())

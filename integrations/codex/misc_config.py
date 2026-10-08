"""Prepare a reviewable device-wide misc default; never install or contact an owner."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import sys

from librarian_policy import resolve
from misc_binding import canonical_path, validate_excluded_roots
from service import load_config, server_database_path
from target_policy import MISC_SCHEMA, ULID

FIELDS = {"schema", "state_dir", "service_config", "memory_scope", "memory_mode", "reader_model",
          "librarian_effort", "recording_mode", "reader_codex", "reader_codex_sha256", "store_target",
          "excluded_roots"}
MAX_STATIC_CONFIG_BYTES = 8000  # hooks.MAX_CONFIG; no larger authoring envelope.


def validate_config(value):
    """Validate the static device contract, without binding or contacting a workspace."""
    if (not isinstance(value, dict) or set(value) not in (FIELDS, FIELDS | {"reader_auth"})
            or value.get("schema") != MISC_SCHEMA or value.get("memory_scope") != "misc"
            or value.get("memory_mode") != "async" or value.get("recording_mode") not in ("off", "automatic")):
        raise ValueError("misc requires static v11 async configuration without project_root")
    resolve(value)
    for field in ("state_dir", "service_config", "reader_codex", "reader_auth"):
        if field in value:
            if not isinstance(value[field], str):
                raise ValueError("misc configuration paths must be strings")
            canonical_path(value[field])
    if (not isinstance(value.get("reader_codex_sha256"), str)
            or re.fullmatch(r"[0-9a-f]{64}", value["reader_codex_sha256"]) is None):
        raise ValueError("invalid reader_codex_sha256")
    if not isinstance(value["excluded_roots"], list):
        raise ValueError("excluded_roots must be a JSON array")
    validate_excluded_roots(value["excluded_roots"])
    target = value["store_target"]
    if (not isinstance(target, dict) or set(target) != {"db_alias", "database_path", "db_id"}
            or target.get("db_alias") != "project" or not isinstance(target.get("db_id"), str)
            or ULID.fullmatch(target["db_id"]) is None):
        raise ValueError("misc requires an explicit project store path and canonical db_id")
    server_database_path(target["database_path"])
    if len(json.dumps(value, ensure_ascii=False, indent=2).encode()) > MAX_STATIC_CONFIG_BYTES:
        raise ValueError("misc configuration exceeds hook configuration byte limit")
    return dict(value)


def prepare(*, state_dir, service_config, database_path, db_id, reader_codex,
            hooks_program, hook_config_path, excluded_roots=(), python=sys.executable,
            librarian_effort="medium", recording_mode="automatic", reader_auth=None):
    service = canonical_path(service_config)
    reader = canonical_path(Path(reader_codex).resolve(strict=True))
    program = canonical_path(Path(hooks_program).resolve(strict=True))
    # Python installations commonly expose their executable via a symlink.
    # Pin the executable itself; workspace bindings separately retain lexical
    # origin and canonical root so aliases cannot erase privacy boundaries.
    interpreter = canonical_path(Path(python).resolve(strict=True))
    state = canonical_path(state_dir)
    destination = canonical_path(hook_config_path)
    if (not service.is_file() or not reader.is_file() or not os.access(reader, os.X_OK)
            or not program.is_file() or not interpreter.is_file() or not os.access(interpreter, os.X_OK)):
        raise ValueError("misc preparation requires existing service, hook program, executable reader and Python")
    config = {"schema": MISC_SCHEMA, "state_dir": str(state), "service_config": str(service),
              "memory_scope": "misc", "memory_mode": "async", "reader_model": "gpt-6.1-sol",
              "librarian_effort": librarian_effort, "recording_mode": recording_mode,
              "reader_codex": str(reader), "reader_codex_sha256": hashlib.sha256(reader.read_bytes()).hexdigest(),
              "store_target": {"db_alias": "project", "database_path": database_path, "db_id": db_id},
              "excluded_roots": list(excluded_roots)}
    if reader_auth is not None:
        config["reader_auth"] = str(canonical_path(reader_auth))
    validate_config(config)
    owner = load_config(service)  # Configuration inspection only; never connect/start.
    if owner.database_name != "project" or str(owner.database_path) != database_path:
        raise ValueError("misc target differs from service configuration")
    from install import PROGRAMS, hook_handlers
    command = shlex.join([str(interpreter), str(program), "--config", str(destination)])
    sources = Path(__file__).parent
    return {"schema": "mneme.codex-misc-prepare.v1", "hook_config": config,
            "global_hooks": {"hooks": hook_handlers(command, async_mode=True, recording=recording_mode == "automatic")},
            "hook_config_path": str(destination),
            "service_config_sha256": hashlib.sha256(service.read_bytes()).hexdigest(),
            "programs": [{"name": name, "sha256": hashlib.sha256((sources / name).read_bytes()).hexdigest()}
                         for name in PROGRAMS],
            "installation": "not performed", "service": "not contacted",
            "session_policy": "quiesce old sessions; fresh session only; retain prior accounting unchanged"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ("state-dir", "service-config", "database-path", "db-id", "reader-codex",
                 "hooks-program", "hook-config-path"):
        parser.add_argument("--" + flag, required=True)
    parser.add_argument("--excluded-root", action="append", default=[], dest="excluded_roots")
    parser.add_argument("--python", default=sys.executable)
    parser.add_argument("--librarian-effort", choices=("low", "medium", "high"), default="medium")
    parser.add_argument("--recording-mode", choices=("off", "automatic"), default="automatic")
    parser.add_argument("--reader-auth")
    print(json.dumps(prepare(**vars(parser.parse_args())), ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()

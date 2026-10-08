"""Prepare a reviewed workshop hook config; never install or contact a host."""
import argparse
import hashlib
import json
import os
from pathlib import Path

from target_policy import workshop_policy, WORKSHOP_SCHEMA


def prepare(*, workspace_root, state_dir, service_config, database_path, db_id, reader_codex):
    root = Path(workspace_root).resolve(strict=True)
    state = Path(state_dir)
    service = Path(service_config).resolve(strict=True)
    reader = Path(reader_codex).resolve(strict=True)
    if (not root.is_dir() or not state.is_absolute() or not reader.is_file()
            or not os.access(reader, os.X_OK)):
        raise ValueError("workshop requires existing workspace and executable, absolute state directory")
    config = {"schema": WORKSHOP_SCHEMA, "project_root": str(root), "state_dir": str(state),
              "service_config": str(service), "memory_scope": "workshop", "memory_mode": "async",
              "reader_model": "gpt-6.1-sol", "librarian_effort": "medium", "recording_mode": "automatic",
              "reader_codex": str(reader), "reader_codex_sha256": hashlib.sha256(reader.read_bytes()).hexdigest(),
              "store_target": {"db_alias": "user", "database_path": database_path, "db_id": db_id}}
    workshop_policy(config)
    from install import PROGRAMS
    sources = Path(__file__).parent
    return {"schema": "mneme.codex-workshop-prepare.v1", "hook_config": config,
            "service_config_sha256": hashlib.sha256(service.read_bytes()).hexdigest(),
            "programs": [{"name": name, "sha256": hashlib.sha256((sources / name).read_bytes()).hexdigest()}
                         for name in PROGRAMS],
            "installation": "not performed", "service": "not contacted",
            "session_policy": "quiesce old sessions; fresh session only; retain prior accounting unchanged"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ("workspace-root", "state-dir", "service-config", "database-path", "db-id", "reader-codex"):
        parser.add_argument("--" + flag, required=True)
    args = parser.parse_args()
    print(json.dumps(prepare(**vars(args)), ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()

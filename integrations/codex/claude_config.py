"""Explicit, review-only Claude capture adapter configuration.

No ~/.claude discovery, credential copying, hook installation or Codex executable.
Native TOML selection remains the shared owner/workspace authority.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import sys

from host_platform import hook_command, is_executable
from misc_binding import canonical_path
from rollout_primitives import decode_json, read_regular
from registry_runtime import native_memory_config_path

SCHEMA = "mneme.claude-hooks.config.v1"
MAX_CONFIG = 8192
FIELDS = {"schema", "source_host", "assessor", "state_dir", "memory_config", "native_cli", "native_cli_sha256",
          "reader_claude", "reader_claude_sha256", "reader_model", "librarian_effort", "transcripts_root"}


def _hash(path):
    # Executable bytes are explicit machinery, not transcript or credential input.
    h = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def validate_static(value):
    if not isinstance(value, dict) or set(value) != FIELDS or value.get("schema") != SCHEMA:
        raise ValueError("unsupported Claude capture configuration; prepare explicit v1 adapter")
    if value["source_host"] != "claude_code" or value["assessor"] != "claude_code":
        raise ValueError("Claude capture requires explicit Claude source and assessor")
    if not isinstance(value["memory_config"], str):
        raise ValueError("memory_config must be an explicit path string")
    native_memory_config_path(value["memory_config"])
    for key in ("state_dir", "native_cli", "reader_claude", "transcripts_root"):
        if not isinstance(value[key], str):
            raise ValueError("adapter paths must be explicit strings")

        if str(canonical_path(value[key])) != value[key]:
            raise ValueError("adapter paths must be normalized absolute paths")
    for key in ("native_cli_sha256", "reader_claude_sha256"):
        if not isinstance(value[key], str) or not re.fullmatch(r"[0-9a-f]{64}", value[key]):
            raise ValueError("adapter requires exact executable SHA256")
    from librarian_policy import resolve
    resolve(value)  # Same strict assessor/model/effort contract, before native selection.
    return dict(value)


def load_config(path, *, cwd):
    path = canonical_path(path)
    raw = read_regular(path, MAX_CONFIG)
    if len(raw) > MAX_CONFIG:
        raise ValueError("Claude adapter config byte limit")
    value = validate_static(decode_json(raw))
    for key in ("native_cli", "reader_claude"):
        executable = Path(value[key])
        if not is_executable(executable) or _hash(executable) != value[key + "_sha256"]:
            raise ValueError("Claude adapter executable unavailable or changed; reprepare")
    root = Path(value["transcripts_root"])
    if root.resolve(strict=True) != root or not root.is_dir():
        raise ValueError("explicit transcripts_root must be a real directory")
    from registry_runtime import native_capture_selection
    config = native_capture_selection(value, path, raw, cwd=cwd, capture_only=True)
    # Machinery is not policy: don't silently substitute the native Codex model.
    config.update(reader_model=value["reader_model"], librarian_effort=value["librarian_effort"],
                  source_host="claude_code", assessor="claude_code", recall_mode="off", _background_maintenance=False)
    for key in ("state_dir", "native_cli", "reader_claude", "transcripts_root"):
        config[key] = Path(config[key])
    from recording_jobs import _config_digest
    config["_reader_config_pin"] = _config_digest(config)
    return config


def prepare(*, memory_config, native_cli, state_dir, reader_claude, reader_model, librarian_effort,
            transcripts_root, hooks_program, hook_config_path, python=sys.executable):
    """Return config and Claude settings snippet; never install or contact owners."""
    paths = {key: canonical_path(Path(value).resolve(strict=True) if key in ("native_cli", "reader_claude", "python", "hooks_program") else value) for key, value in {
        "native_cli": native_cli, "reader_claude": reader_claude, "state_dir": state_dir,
        "transcripts_root": transcripts_root, "hooks_program": hooks_program,
        "hook_config_path": hook_config_path, "python": python}.items()}
    if not all(is_executable(paths[key]) for key in ("native_cli", "reader_claude", "python")):
        raise ValueError("prepare requires explicit existing executable paths")
    if paths["hooks_program"].name != "claude_hooks.py" or not paths["hooks_program"].is_file():
        raise ValueError("prepare requires shipped claude_hooks.py")
    config = validate_static({"schema": SCHEMA, "source_host": "claude_code", "assessor": "claude_code",
        "memory_config": str(native_memory_config_path(memory_config)),
        **{key: str(paths[key]) for key in ("native_cli", "reader_claude", "state_dir", "transcripts_root")},
        "native_cli_sha256": _hash(paths["native_cli"]), "reader_claude_sha256": _hash(paths["reader_claude"]),
        "reader_model": reader_model, "librarian_effort": librarian_effort})
    if len(json.dumps(config).encode()) > MAX_CONFIG:
        raise ValueError("Claude adapter config byte limit")
    command = hook_command([str(paths["python"]), str(paths["hooks_program"]), "--config", str(paths["hook_config_path"])])
    events = ("SessionStart", "UserPromptSubmit", "Stop", "StopFailure", "SessionEnd", "PreCompact")
    return {"schema": "mneme.claude-capture-prepare.v1", "hook_config": config,
            "hook_config_path": str(paths["hook_config_path"]),
            "claude_settings": {"hooks": {event: [{"hooks": [{"type": "command", "command": command, "timeout": 10}]}]
                                          for event in events}},
            "installation": "not performed", "services": "not contacted",
            "scope": "capture only; no selector, semantic recall, or startup core wiring",
            "session_policy": "review and enable separately; start fresh sessions; retain old ledgers"}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ("memory-config", "native-cli", "state-dir", "reader-claude", "reader-model", "librarian-effort",
                "transcripts-root", "hooks-program", "hook-config-path"):
        parser.add_argument("--" + key, required=True)
    parser.add_argument("--python", default=sys.executable)
    args = parser.parse_args(argv)
    try:
        print(json.dumps(prepare(**vars(args)), ensure_ascii=False, indent=2))
        return 0
    except (OSError, ValueError, TypeError):
        print(json.dumps({"error": "invalid Claude adapter preparation inputs; use explicit reviewed paths"}))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

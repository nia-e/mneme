#!/usr/bin/env python3
"""Dispatch the one device misc librarian only for unconfigured workspaces.

Project hooks are additive in Codex. This front door therefore declines their
workspaces before loading the misc owner or starting the existing lifecycle.
"""
import argparse
import json
from pathlib import Path
import sys

import hooks
from misc_binding import choose_workspace
from misc_config import validate_config

EVENTS = hooks.HOOK_EVENTS


def handle_event(event, config_path, *, reader_background=False):
    if (not isinstance(event, dict) or event.get("hook_event_name") not in EVENTS
            or hooks._subagent(event) or not hooks._valid_id(event.get("session_id"))):
        return {}
    if event["hook_event_name"] == "SessionStart" and event.get("source") not in ("startup", "resume", "clear", "compact"):
        return {}
    static = validate_config(hooks._bounded_json(Path(config_path), hooks.MAX_CONFIG))
    binding = choose_workspace(event.get("cwd"), excluded_roots=static["excluded_roots"])
    if binding is None:
        return {}
    config = hooks._config(Path(config_path), workspace_binding=binding)
    return hooks.handle_event(event, config, reader_background=reader_background)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, type=Path)
    parser.add_argument("--reader-background", action="store_true")
    args = parser.parse_args(argv)
    try:
        event = hooks._read_event(sys.stdin.buffer)
        result = ({} if event is None else
                  handle_event(event, args.config, reader_background=args.reader_background))
    except hooks.HookInputError as exc:
        warning = hooks._input_warning(str(exc))
        if args.reader_background:
            print(warning["systemMessage"], file=sys.stderr)
        result = {} if args.reader_background else warning
    except (hooks.HookError, OSError, ValueError, TypeError, RecursionError):
        result = {} if args.reader_background else hooks._warning()
    print(json.dumps(result, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

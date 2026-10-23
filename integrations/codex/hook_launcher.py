#!/usr/bin/env python3
"""Dispatch one device actor; native configuration selects applicable stores.

Legacy misc/registry configs still decline configured workspaces. Native configs
cover all workspaces and must replace, not duplicate, project hook actors.
"""
import argparse
import json
from pathlib import Path
import sys

import hooks
from misc_binding import choose_workspace
from misc_config import validate_config

EVENTS = hooks.HOOK_EVENTS


def _core_event(event, config):
    import core_hook
    try:
        prepared = core_hook.handle_hook_event(event, config, with_activity=True)
    except Exception:
        hooks._activity_emit(event, "core", "unavailable", reason="selection_unavailable",
                             selection_digest=hooks._activity_selection_digest(config))
        raise
    if isinstance(prepared, core_hook.PreparedCore):
        # Core collected/revalidated once and returned its body-free projection.
        # Publication is deliberately outside the passive collector.
        try:
            hooks._activity_core(event, prepared)
        except Exception:
            pass
        return prepared.output
    return prepared


def handle_event(event, config_path, *, reader_background=False, core=False):
    from store_registry import native_selection_budget
    # Match the existing recipe ceilings (15s core, 5s ordinary, 3s shutdown).
    # Leave room for host/state work; nested loads and replays share this bound.
    short = isinstance(event, dict) and event.get("hook_event_name") in ("Interrupt", "SessionEnd")
    with hooks._defer_activity() as activity_batch:
        with native_selection_budget(12 if core else 1.5 if short else 3):
            result = _handle_event(event, config_path, reader_background=reader_background, core=core)
    if not reader_background and not core:
        result = hooks._activity_startup_marker(event, result, bool(getattr(activity_batch, "recorded_lifecycle", False)))
    return result


def _handle_event(event, config_path, *, reader_background=False, core=False):
    if (not isinstance(event, dict) or event.get("hook_event_name") not in EVENTS
            or hooks._subagent(event) or not hooks._valid_id(event.get("session_id"))):
        return {}
    if event["hook_event_name"] == "SessionStart" and event.get("source") not in ("startup", "resume", "clear", "compact"):
        return {}
    if core and (reader_background or event["hook_event_name"] != "SessionStart"):
        return {}
    raw = hooks._bounded_json(Path(config_path), hooks.MAX_CONFIG)
    if isinstance(raw, dict) and raw.get("schema") == "mneme.codex-hooks.config.v13":
        # No Python discovery/selection here. Disabled, isolated, exclusions and
        # per-operation permissions come from the bounded native selection.
        config = hooks._config(Path(config_path), cwd=event.get("cwd"))
        if core:
            return _core_event(event, config)
        return hooks.handle_event(event, config, reader_background=reader_background)
    if isinstance(raw, dict) and raw.get("schema") == hooks.CONFIG_SCHEMA_V12:
        from registry_runtime import validate_static
        static = validate_static(raw)
        if static.get("memory_scope") != "registry":
            raise ValueError("global dispatcher requires registry scope")
        # Legacy enrollment and even an empty .mneme directory are boundaries,
        # not permission to start another session actor from the global hook.
        binding = choose_workspace(event.get("cwd"))
        if binding is None:
            return {}
        config = hooks._config(Path(config_path), workspace_binding=binding, cwd=event["cwd"])
        if core:
            return _core_event(event, config)
        return hooks.handle_event(event, config, reader_background=reader_background)
    if core:
        raise ValueError("core dispatcher requires explicit registry configuration")
    static = validate_config(raw)
    binding = choose_workspace(event.get("cwd"), excluded_roots=static["excluded_roots"])
    if binding is None:
        return {}
    config = hooks._config(Path(config_path), workspace_binding=binding)
    return hooks.handle_event(event, config, reader_background=reader_background)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, type=Path)
    parser.add_argument("--reader-background", action="store_true")
    parser.add_argument("--core", action="store_true", help="passive selected-store core; independent of background recording")
    args = parser.parse_args(argv)
    event = None
    try:
        event = hooks._read_event(sys.stdin.buffer)
        result = ({} if event is None else
                  handle_event(event, args.config, reader_background=args.reader_background, **({"core": True} if args.core else {})))
    except hooks.HookInputError as exc:
        warning = hooks._input_warning(str(exc))
        if args.reader_background:
            print(warning["systemMessage"], file=sys.stderr)
        result = {} if args.reader_background else warning
    except (hooks.HookError, OSError, ValueError, TypeError, RecursionError):
        result = hooks._lifecycle_failure(event, background=args.reader_background)
    print(json.dumps(result, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

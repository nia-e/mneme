#!/usr/bin/env python3
"""Stage and select a POSIX device-hook runtime behind one fixed real launcher.

The native hook command is always /bin/sh <stable-root>/launch. Its implementation
is deliberately mutable through <stable-root>/current, an atomic symlink to one
absolute runtime wrapper path. No command is evaluated or discovered on PATH/cwd.

Install only stages files; select is the explicit atomic activation (and rollback)
operation. Neither grants native hook trust, touches an owner or lifecycle ledger,
nor migrates running workers. Retain prior bundles and plans until work drains.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import secrets
import shlex
import stat
import tempfile

import host_files

PLAN_SCHEMA = "mneme.codex-stable-hooks.plan.v1"
RESULT_SCHEMA = "mneme.codex-stable-hooks.result.v1"
MAX_PATH_BYTES = 4096
MAX_PLAN_BYTES = 32 * 1024
NOTICE = "Automatic memory temporarily disabled: stable runtime unavailable."
_FIELDS = {"schema", "stable_root", "runtime", "python", "hooks_program", "hook_config_path"}


class SelectionUncertain(OSError):
    """Replacement completed, but its directory durability barrier failed."""
    def __init__(self, selector, runtime):
        super().__init__("selector replaced; durability not confirmed; inspect current before retrying")
        self.selector = str(selector)
        self.runtime = str(runtime)


def _posix():
    if os.name != "posix":
        raise ValueError("stable hooks require POSIX /bin/sh; retain direct hooks on Windows")


def _path(value):
    if isinstance(value, Path):
        value = str(value)
    if (not isinstance(value, str) or not value or not value.startswith("/")
            or value.startswith("//") or os.path.normpath(value) != value
            or len(value.encode("utf-8")) > MAX_PATH_BYTES
            or any(ord(char) < 32 or ord(char) == 127 for char in value)):
        raise ValueError("stable hook paths must be bounded normalized absolute paths")
    return Path(value)


def entrypoint_argv(entrypoint):
    """Validate command spelling without resolving links or requiring installation."""
    _posix()
    return ["/bin/sh", str(_path(entrypoint))]


def render_entrypoint(selector):
    """Standalone real shell script; it does not import the updater or old bundle."""
    _posix()
    selector = shlex.quote(str(_path(selector)))
    notice = shlex.quote(NOTICE)
    return f"""#!/bin/sh
# mneme.stable-hook-entrypoint.v1
refuse() {{ printf '%s\\n' {notice} >&2; exit 1; }}
selector={selector}
[ -L "$selector" ] && [ -f "$selector" ] && [ -x "$selector" ] || refuse
exec "$selector" "$@"
""".encode("utf-8")


def render_runtime(python, hooks_program, hook_config_path):
    """Pin one coherent interpreter/dispatcher/config invocation, not source bytes."""
    _posix()
    argv = [str(_path(python)), str(_path(hooks_program)), "--config", str(_path(hook_config_path))]
    return ("#!/bin/sh\n# mneme.stable-hook-runtime.v1\nexec "
            + shlex.join(argv) + ' "$@"\n').encode("utf-8")


def _read(path, limit, *, private=False):
    with os.fdopen(host_files.open_regular(path), "rb") as stream:
        if private:
            host_files.check_private_fd(stream.fileno())
        if os.fstat(stream.fileno()).st_size > limit:
            raise ValueError("stable hook artifact exceeds byte limit")
        raw = stream.read(limit + 1)
        if len(raw) > limit:
            raise ValueError("stable hook artifact exceeds byte limit")
        return raw


def _optional_read(path, limit, *, private=False):
    try:
        return _read(path, limit, private=private)
    except FileNotFoundError:
        return None


def _inputs(plan):
    """Check machinery only; the dispatcher remains the native policy authority."""
    interpreter = Path(plan["python"])
    program = Path(plan["hooks_program"])
    if any(path.resolve(strict=True) != path for path in (interpreter, program)):
        raise ValueError("runtime machinery path changed; prepare a new plan with resolved bundle paths")
    if not interpreter.is_file() or not os.access(interpreter, os.X_OK):
        raise ValueError("stable runtime requires an existing executable Python interpreter")
    if program.name != "hook_launcher.py":
        raise ValueError("stable native hooks require the hook_launcher.py dispatcher")
    _read(program, 1024 * 1024)
    _read(Path(plan["hook_config_path"]), 8000)


def _checked(plan):
    _posix()
    if not isinstance(plan, dict) or set(plan) != _FIELDS or plan.get("schema") != PLAN_SCHEMA:
        raise ValueError("invalid stable hook plan")
    checked = {key: str(_path(plan[key])) for key in _FIELDS - {"schema"}}
    root, runtime = Path(checked["stable_root"]), Path(checked["runtime"])
    _path(root / "launch")
    _path(root / "current")
    outputs = {root / "launch", root / "current", runtime}
    inputs = {Path(checked[key]) for key in ("python", "hooks_program", "hook_config_path")}
    if (len(outputs) != 3 or root in outputs or outputs & inputs
            or root in inputs or runtime in root.parents
            or any(output in other.parents for output in outputs for other in outputs | inputs)):
        raise ValueError("stable hook artifact and input paths must be distinct")
    result = {"schema": PLAN_SCHEMA, **checked}
    if len((_json(result) + "\n").encode("utf-8")) > MAX_PLAN_BYTES:
        raise ValueError("stable hook plan exceeds byte limit")
    return result


def _json(value):
    """The plan admission bound and CLI output use the same serialization."""
    return json.dumps(value, ensure_ascii=False, indent=2)


def prepare(*, stable_root, runtime, python, hooks_program, hook_config_path):
    """Resolve runtime machinery, but never resolve stable command/config spelling."""
    plan = _checked({"schema": PLAN_SCHEMA, "stable_root": str(stable_root),
                     "runtime": str(runtime), "python": str(python),
                     "hooks_program": str(hooks_program), "hook_config_path": str(hook_config_path)})
    for key in ("python", "hooks_program"):
        plan[key] = str(Path(plan[key]).resolve(strict=True))
    plan = _checked(plan)
    _inputs(plan)
    return plan


def _artifacts(plan):
    root = Path(plan["stable_root"])
    return (root / "launch", root / "current",
            render_entrypoint(root / "current"),
            render_runtime(plan["python"], plan["hooks_program"], plan["hook_config_path"]))


def _current(selector):
    try:
        current = selector.lstat()
    except FileNotFoundError:
        return None
    if not stat.S_ISLNK(current.st_mode) or current.st_uid != os.getuid():
        raise ValueError("stable runtime selector must be an owned symlink; preserve it and inspect")
    try:
        return str(_path(os.readlink(selector)))
    except (ValueError, UnicodeError):
        raise ValueError("invalid stable runtime selector; inspect before selecting a runtime") from None


def _publish_selector(selector, runtime):
    """Atomic old/new pointer; directory sync failure after rename is ambiguous."""
    temporary = selector.parent / (".mneme-select-" + secrets.token_hex(16))
    os.symlink(str(runtime), temporary)
    try:
        os.replace(temporary, selector)
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise
    # Successful replacement consumed temporary. A redundant unlink here could
    # mask the actual post-replacement outcome if directory access has changed.
    try:
        host_files.sync_directory(selector.parent)
    except (OSError, ValueError) as exc:
        raise SelectionUncertain(selector, runtime) from exc


def _new_file(path, raw):
    """Publish a complete real 0700 file, never replacing any existing artifact."""
    fd, temporary = tempfile.mkstemp(prefix=".mneme-stage-", dir=path.parent)
    temporary = Path(temporary)
    try:
        with os.fdopen(fd, "wb") as stream:
            os.fchmod(stream.fileno(), 0o700)
            stream.write(raw)
            stream.flush()
            os.fsync(stream.fileno())
        host_files.rename_no_replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def _result(operation, plan, previous):
    root = Path(plan["stable_root"])
    return {"schema": RESULT_SCHEMA, "operation": operation,
            "status": "staged" if operation == "install" else "selected",
            "entrypoint": str(root / "launch"), "selector": str(root / "current"),
            "runtime": plan["runtime"], "previous_runtime": previous}


def install(plan):
    """Stage a NEW wrapper and create/reuse the exact shim; do not select it.

    A failed stage may leave unselected complete artifacts. Never erase those or
    overwrite them on retry: inspect the reported paths and choose a fresh wrapper.
    """
    plan = _checked(plan)
    _inputs(plan)
    entrypoint, selector, shim, wrapper = _artifacts(plan)
    runtime = Path(plan["runtime"])
    existing = _optional_read(entrypoint, MAX_PLAN_BYTES, private=True)
    if existing is not None and existing != shim:
        raise ValueError("stable entrypoint differs; preserve it and inspect before installing")
    previous = _current(selector)
    if previous is not None and existing is None:
        raise ValueError("stable selector exists without its entrypoint; inspect the partial installation")
    if os.path.lexists(runtime):
        raise ValueError("runtime wrapper already exists; preserve it and choose a new runtime path")
    host_files.ensure_private_directory(Path(plan["stable_root"]))
    host_files.ensure_private_directory(runtime.parent)
    _new_file(runtime, wrapper)
    if existing is None:
        _new_file(entrypoint, shim)
    return _result("install", plan, previous)


def select(plan):
    """Activate a staged plan atomically; selecting a retained plan rolls back.

    previous_runtime is the observed pre-switch target, not a concurrent-writer
    compare-and-swap guarantee. Use one operator for installation/selection.
    """
    plan = _checked(plan)
    _inputs(plan)
    entrypoint, selector, shim, wrapper = _artifacts(plan)
    host_files.check_private_directory(Path(plan["stable_root"]))
    runtime = Path(plan["runtime"])
    if _read(entrypoint, MAX_PLAN_BYTES, private=True) != shim:
        raise ValueError("stable entrypoint differs; inspect before selecting a runtime")
    if _read(runtime, MAX_PLAN_BYTES, private=True) != wrapper or not os.access(runtime, os.X_OK):
        raise ValueError("runtime wrapper differs from its plan; stage a new runtime")
    previous = _current(selector)
    _publish_selector(selector, runtime)
    return _result("select", plan, previous)


def _unique(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError("duplicate stable hook plan field")
        value[key] = item
    return value


def _plan_file(path):
    return _checked(json.loads(_read(path, MAX_PLAN_BYTES), object_pairs_hook=_unique))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="operation", required=True)
    preview = commands.add_parser("prepare", help="print an install plan without changing files")
    for name in ("stable-root", "runtime", "python", "hooks-program", "hook-config-path"):
        preview.add_argument("--" + name, required=True)
    for name, help_text in (("install", "stage a new wrapper; do not activate it"),
                            ("select", "activate a staged plan or roll back to a retained plan")):
        operation = commands.add_parser(name, help=help_text)
        operation.add_argument("--plan", required=True, type=Path)
    args = parser.parse_args(argv)
    try:
        if args.operation == "prepare":
            result = prepare(**{key: value for key, value in vars(args).items() if key != "operation"})
        else:
            plan = _plan_file(args.plan)
            result = install(plan) if args.operation == "install" else select(plan)
    except SelectionUncertain as exc:
        print(json.dumps({"schema": RESULT_SCHEMA, "operation": args.operation,
                          "status": "selection_uncertain", "selector": exc.selector,
                          "runtime": exc.runtime, "error": str(exc)}, ensure_ascii=False))
        return 1
    except (OSError, ValueError, TypeError, RecursionError) as exc:
        print(json.dumps({"schema": RESULT_SCHEMA, "operation": args.operation,
                          "status": "refused", "error": str(exc)[:512]}, ensure_ascii=False))
        return 1
    print(_json(result))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

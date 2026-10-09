"""Generate a one-hop compatibility entry point for explicitly inventoried hooks.

This module does not publish files or select owners. The operator must verify the
old/new configs describe the same intended owner and accounting namespace, back
up the old script, and publish with a preimage compare-and-swap. Qualified sibling
imports belong to the target bundle; the script pin alone does not cover them.

Config byte pins are deliberate: even formatting or --no-recording changes make
this historical entry point refuse until its map is explicitly republished. No
runtime state/usage ledger is pinned, reset or rewritten. Pins detect ordinary
changes, not hostile filesystem races between verification and exec.
"""
import json
import os
import re

MARKER = "# mneme.cached-hook-forwarder.v1"
NOTICE = "Automatic memory temporarily disabled."
MAX_SPEC_BYTES = 64 * 1024
_FIELDS = frozenset(("old_config_sha256", "target_script", "target_script_sha256",
                     "target_config", "target_config_sha256"))


def _path(value):
    if (not isinstance(value, str) or not os.path.isabs(value)
            or value.startswith("//") or os.path.normpath(value) != value
            or len(value.encode("utf-8")) > 4096
            or any(ord(char) < 32 or ord(char) == 127 for char in value)):
        raise ValueError("forwarder paths must be bounded normalized absolute paths")
    return value


def _spec(source_script, routes):
    source_script = _path(source_script)
    if not isinstance(routes, dict) or not routes:
        raise ValueError("forwarder requires an explicit nonempty config allowlist")
    checked = {}
    for old_config, route in sorted(routes.items()):
        _path(old_config)
        if not isinstance(route, dict) or set(route) != _FIELDS:
            raise ValueError("invalid forwarder route fields")
        for field in ("target_script", "target_config"):
            _path(route[field])
        for field in ("old_config_sha256", "target_script_sha256", "target_config_sha256"):
            if not isinstance(route[field], str) or re.fullmatch(r"[0-9a-f]{64}", route[field]) is None:
                raise ValueError("forwarder pins must be lowercase SHA-256")
        if route["target_script"] == source_script:
            raise ValueError("forwarder must target a different direct entry point")
        if source_script in (old_config, route["target_config"]):
            raise ValueError("forwarder script cannot also be a config")
        checked[old_config] = {field: route[field] for field in sorted(_FIELDS)}
    if len(json.dumps([source_script, checked], ensure_ascii=False).encode()) > MAX_SPEC_BYTES:
        raise ValueError("forwarder mapping exceeds its byte allowance")
    return source_script, checked


# Standalone generated code: it must not import a helper from the retired bundle.
_RUNTIME = r'''
import hashlib
import os
import stat
import sys


def _read_pinned(path, expected, limit):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        before = os.fstat(stream.fileno())
        if not stat.S_ISREG(before.st_mode) or before.st_nlink != 1 or before.st_size > limit:
            raise ValueError("file admission")
        raw = stream.read(limit + 1)
        after = os.fstat(stream.fileno())
    current = os.stat(path, follow_symlinks=False)
    identity = lambda s: (s.st_dev, s.st_ino, s.st_size, s.st_mtime_ns, s.st_ctime_ns)
    if (len(raw) > limit or identity(before) != identity(after)
            or identity(after) != identity(current) or not stat.S_ISREG(current.st_mode)
            or current.st_nlink != 1 or hashlib.sha256(raw).hexdigest() != expected):
        raise ValueError("file changed")
    return raw


def _forward():
    if __file__ != SOURCE_SCRIPT or sys.argv[0] != SOURCE_SCRIPT:
        raise ValueError("unexpected cached entry point")
    source = os.stat(SOURCE_SCRIPT, follow_symlinks=False)
    if not stat.S_ISREG(source.st_mode) or source.st_nlink != 1:
        raise ValueError("invalid cached entry point")
    args = sys.argv[1:]
    found = None
    index = 0
    while index < len(args):
        arg = args[index]
        name, separator, value = arg.partition("=")
        if name == "--config":
            if found is not None:
                raise ValueError("duplicate config")
            if not separator:
                if index + 1 == len(args):
                    raise ValueError("missing config value")
                index += 1
                value = args[index]
            if value not in ROUTES:
                raise ValueError("unrecognized config")
            found = (index, bool(separator), value)
        elif name.startswith("--") and ("--config".startswith(name) or name.startswith("--config")):
            # argparse's --c/--conf aliases otherwise override our mapped flag.
            raise ValueError("noncanonical config option")
        index += 1
    if found is None:
        raise ValueError("missing config")
    index, equals, old_config = found
    route = ROUTES[old_config]
    _read_pinned(old_config, route["old_config_sha256"], 8000)
    _read_pinned(route["target_config"], route["target_config_sha256"], 8000)
    program = _read_pinned(route["target_script"], route["target_script_sha256"], 1024 * 1024)
    if MARKER.encode() in program.splitlines()[:3]:
        raise ValueError("forwarder chains are not supported")
    args[index] = ("--config=" if equals else "") + route["target_config"]
    if not os.path.isabs(sys.executable):
        raise ValueError("Python interpreter unavailable")
    os.execv(sys.executable, [sys.executable, route["target_script"], *args])


if __name__ == "__main__":
    try:
        _forward()
    except Exception:
        print(NOTICE, file=sys.stderr)
        raise SystemExit(1)
'''


def render_forwarder(source_script, routes):
    """Return deterministic UTF-8 script bytes; do not read or change live files.

    routes maps each exact old --config path to all five required _FIELDS. Paths
    and pins are explicit caller evidence, never inferred by searching a runtime
    directory. A misc route must name hook_launcher.py, not bypass its dispatcher.
    All non-config argv, stdin, cwd and environment pass through untouched.
    """
    source_script, routes = _spec(source_script, routes)
    header = (f"#!/usr/bin/env python3\n{MARKER}\n"
              f"SOURCE_SCRIPT = {source_script!r}\nROUTES = {routes!r}\n"
              f"MARKER = {MARKER!r}\nNOTICE = {NOTICE!r}\n")
    return (header + _RUNTIME).encode("utf-8")


def validate_forwarder(raw, source_script, routes):
    """Require the exact generated artifact, including paths, pins and runtime."""
    if not isinstance(raw, bytes) or raw != render_forwarder(source_script, routes):
        raise ValueError("forwarder does not match the reviewed mapping")

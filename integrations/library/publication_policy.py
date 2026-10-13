"""Explicit native metadata binding for the independent snapshot publisher.

No TOML policy, owner startup, store reads, or publication authority lives here.
Enrollment remains separate; this adapter only narrows existing publication.
"""

import json
import re
from pathlib import Path
import subprocess
import sys
import time

try:
    import host_process
except ModuleNotFoundError as error:
    if error.name != "host_process":
        raise
    helper_dir = Path(__file__).resolve().parent.parent / "codex"
    if not (helper_dir / "host_process.py").is_file():
        raise
    sys.path.insert(0, str(helper_dir))
    import host_process

MAX_SELECTION_BYTES = 512 * 1024
SELECTION_TIMEOUT = 3
# One timer run may recheck several projects and publication boundaries. Charge
# all native selection subprocesses to one finite time/decoded-byte envelope.
MAX_SELECTION_WORK_BYTES = 4 * 1024 * 1024
MAX_SELECTION_WORK_SECONDS = 30
_ID = re.compile(r"[0-7][0-9A-HJKMNP-TV-Z]{25}\Z")
_NAME = re.compile(r"[A-Za-z0-9._-]{1,128}\Z")


def _path(value):
    value = str(value)
    if not value or len(value.encode()) > 4096 or "\0" in value or not Path(value).is_absolute():
        raise ValueError("native publication policy requires bounded absolute paths")
    return Path(value)


def _unique(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError("duplicate native selection key")
        value[key] = item
    return value


def _reject_constant(_):
    raise ValueError("non-JSON value in native publication selection")


def _identity(path):
    value = path.stat()
    return value.st_dev, value.st_ino, value.st_size, value.st_mtime_ns, value.st_ctime_ns


class NativePublicationPolicy:
    def __init__(self, native_cli, memory_config):
        self.native_cli = _path(native_cli)
        self.memory_config = _path(memory_config)
        self._remaining_bytes = MAX_SELECTION_WORK_BYTES
        self._remaining_seconds = MAX_SELECTION_WORK_SECONDS

    def _select(self, project_root):
        root = _path(project_root)
        before = _identity(self.native_cli)
        if self._remaining_bytes <= 0 or self._remaining_seconds <= 0:
            raise ValueError("native publication selection work budget exhausted")
        started = time.monotonic()
        deadline = started + min(SELECTION_TIMEOUT, self._remaining_seconds)
        limit = min(MAX_SELECTION_BYTES, self._remaining_bytes)
        process = host_process.spawn(
            [str(self.native_cli), "--json", "config", "select", "--config",
             str(self.memory_config), "--cwd", str(root)],
            tree=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL)
        pipe = None
        try:
            pipe = host_process.BoundedPipe(process, max_read_bytes=16384)
            process.stdin.close()
            output = bytearray()
            while True:
                chunk = pipe.read(min(16384, limit + 1 - len(output)), deadline)
                if not chunk:
                    break
                output.extend(chunk)
                self._remaining_bytes -= len(chunk)
                if len(output) > limit:
                    raise ValueError("native publication selection exceeds byte limit")
            process.wait(timeout=max(.001, deadline - time.monotonic()))
            if process.returncode or _identity(self.native_cli) != before:
                raise ValueError("native publication selection unavailable or helper changed")
            try:
                return json.loads(output, object_pairs_hook=_unique,
                                  parse_constant=_reject_constant)
            except RecursionError as error:
                raise ValueError("native publication selection nesting exceeds parser limit") from error
        finally:
            if pipe is not None:
                pipe.close()
            host_process.terminate_tree(process, timeout=.2)
            self._remaining_seconds -= time.monotonic() - started

    def allows(self, project_root, catalog_path, db_id, database):
        root = _path(project_root).resolve(strict=True)
        catalog = _path(catalog_path).resolve()
        if (not isinstance(db_id, str) or not _ID.fullmatch(db_id)
                or not isinstance(database, str) or not _NAME.fullmatch(database)):
            raise ValueError("native publication requires canonical database identity and alias")
        selected = self._select(root)
        if (not isinstance(selected, dict)
                or selected.get("schema") != "mneme.config-selection.v1"
                or not isinstance(selected.get("digest"), str)
                or not re.fullmatch(r"[0-9a-f]{64}", selected["digest"])
                or selected.get("profile") not in ("default", "private", "isolated")
                or not isinstance(selected.get("cwd"), str)
                or _path(selected["cwd"]).resolve(strict=True) != root
                or not isinstance(selected.get("libraries"), dict)
                or len(selected["libraries"]) > 32
                or not isinstance(selected.get("stores"), list)
                or len(selected["stores"]) > 64):
            raise ValueError("invalid native publication selection")
        # Validate the complete consumed projection before returning a policy
        # denial: malformed output is unavailable evidence, never withdrawal.
        catalogs = []
        for name, library in selected["libraries"].items():
            if (not isinstance(name, str) or not _NAME.fullmatch(name) or not isinstance(library, dict)
                    or not isinstance(library.get("catalog"), str)):
                raise ValueError("invalid native publication library selection")
            catalogs.append(_path(library["catalog"]).resolve())
        names = set()
        matches = []
        for row in selected["stores"]:
            if not isinstance(row, dict):
                raise ValueError("invalid native publication store selection")
            store, eligible = row.get("store"), row.get("eligible")
            if (not isinstance(row.get("name"), str) or not _NAME.fullmatch(row["name"])
                    or row["name"] in names or type(row.get("active")) is not bool
                    or type(row.get("admitted")) is not bool
                    or not isinstance(store, dict) or not isinstance(eligible, dict)
                    or type(eligible.get("read")) is not bool
                    or not isinstance(store.get("id"), str) or not _ID.fullmatch(store["id"])
                    or not isinstance(store.get("owner"), dict)
                    or not isinstance(store["owner"].get("db"), str)
                    or not _NAME.fullmatch(store["owner"]["db"])
                    or (row["active"] or eligible["read"]) and not row["admitted"]):
                raise ValueError("invalid native publication store selection")
            names.add(row["name"])
            if store["id"] == db_id:
                matches.append(row)
        if len(matches) > 1:
            raise ValueError("ambiguous native publication database identity")
        return (selected["profile"] == "default" and catalog in catalogs and len(matches) == 1
                and matches[0]["store"]["owner"]["db"] == database
                and matches[0]["active"] and matches[0]["admitted"]
                and matches[0]["eligible"]["read"])

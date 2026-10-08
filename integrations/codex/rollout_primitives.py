"""Stateless strict-JSON and file/reference primitives for local rollout readers.

No lifecycle, delivery, authority, database, provider or discovery policy lives here.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import stat
from typing import Any

TOKEN = re.compile(r"[A-Za-z0-9_.:-]{1,256}\Z")
META = "internal_chat_message_metadata_passthrough"
CALL_FIELDS = {"custom_tool_call": "input", "function_call": "arguments"}
OUTPUT_TYPES = {"custom_tool_call_output": "custom_tool_call",
                "function_call_output": "function_call"}


def digest(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def encoded(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True,
                      separators=(",", ":"), allow_nan=False).encode("utf-8")


def _unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate_json_key")
        result[key] = value
    return result


def decode_json(raw: bytes):
    def invalid_constant(_):
        raise ValueError("non_json_number")
    return json.loads(raw.decode("utf-8"), object_pairs_hook=_unique_object,
                      parse_constant=invalid_constant)


def is_token(value) -> bool:
    return isinstance(value, str) and TOKEN.fullmatch(value) is not None


def record_ref(row: dict, raw: bytes, ordinal: int, offset: int) -> dict:
    payload = row["payload"]
    reference = {"ordinal": ordinal, "line": ordinal + 1, "byte_offset": offset,
                 "line_bytes": len(raw), "raw_line_sha256": digest(raw)}
    if is_token(payload.get("id")):
        reference["item_id"] = payload["id"]
    return reference


def open_regular(path: Path) -> int:
    """Return an owned nonblocking descriptor; caller must close it."""
    flags = os.O_RDONLY | os.O_NONBLOCK | getattr(os, "O_NOFOLLOW", 0)
    fd = os.open(path, flags)
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            raise ValueError("not_regular_file")
    except BaseException:
        os.close(fd)
        raise
    return fd


def read_regular(path: Path, limit: int) -> bytes:
    """One bounded read, including a sentinel byte; never discover a path."""
    fd = open_regular(path)
    try:
        with os.fdopen(fd, "rb", closefd=False) as stream:
            return stream.read(limit + 1)
    finally:
        os.close(fd)

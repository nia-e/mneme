#!/usr/bin/env python3
"""Derive a readable collision-resistant stable key from exact identity parts."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import unicodedata


PREFIX = re.compile(r"^[a-z][a-z0-9.-]{0,31}$")


def derive(prefix: str, parts: list[str]) -> str:
    normalized = [unicodedata.normalize("NFC", part) for part in parts]
    identity = json.dumps(
        normalized,
        ensure_ascii=True,
        allow_nan=False,
        separators=(",", ":"),
    ).encode("utf-8")
    # 80 bits keeps birthday collisions negligible even for very large catalogs;
    # the prior 48-bit suffix was only comfortable for tiny per-repo graphs.
    digest = hashlib.sha256(identity).hexdigest()[:20]
    readable = "-".join(normalized)
    readable = unicodedata.normalize("NFKD", readable).encode("ascii", "ignore").decode("ascii")
    readable = re.sub(r"[^a-z0-9]+", "-", readable.lower()).strip("-")
    readable = readable[:48].rstrip("-") or "item"
    return f"{prefix}:{readable}-{digest}"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("prefix")
    parser.add_argument("identity", nargs="+")
    args = parser.parse_args()
    if PREFIX.fullmatch(args.prefix) is None:
        parser.error("prefix must match [a-z][a-z0-9.-]{0,31}")
    print(derive(args.prefix, args.identity))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Select the newest complete locally installed Codex package; never download.

Package editions must remain immutable while running. Selection is a local layout
check, not archive authentication or a replacement for upgrade provenance review.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import sys


class SelectionError(ValueError):
    pass


def _metadata(path: Path) -> dict:
    with path.open("rb") as stream:
        data = stream.read(65537)
    if len(data) > 65536:
        raise SelectionError("package metadata exceeds 64 KiB")
    value = json.loads(data)
    if not isinstance(value, dict):
        raise SelectionError("package metadata must be an object")
    return value


def _version_key(name: str):
    match = re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?", name)
    if match is None:
        return None
    major, minor, patch, prerelease, _build = match.groups()
    identifiers = []
    for identifier in prerelease.split(".") if prerelease else ():
        if identifier.isdecimal():
            if len(identifier) > 1 and identifier.startswith("0"):
                return None
            identifiers.append((0, int(identifier)))
        else:
            identifiers.append((1, identifier))
    # Build metadata has no precedence; name provides deterministic equal-version ties.
    return (int(major), int(minor), int(patch), prerelease is None, tuple(identifiers), name)


def select_package(packages: Path) -> Path:
    """Freeze the newest complete semantic-versioned edition for one launch.

    Incomplete/unsupported editions are ignored. The bounded inventory fails
    closed if too large rather than choosing from an arbitrary partial listing.
    """
    if not packages.is_absolute():
        raise SelectionError("--codex-packages must be absolute")
    packages = packages.resolve(strict=True)
    candidates = []
    with os.scandir(packages) as entries:
        for count, entry in enumerate(entries, 1):
            if count > 1024:
                raise SelectionError("Codex package inventory exceeds 1024 entries")
            version = _version_key(entry.name)
            if version is not None and entry.is_dir(follow_symlinks=False):
                candidates.append((version, Path(entry.path)))
    for _, edition in sorted(candidates, reverse=True):
        try:
            metadata = _metadata(edition / "codex-package.json")
            manifest = _metadata(edition / "release-manifest.json")
            expected = {"layoutVersion": 1, "version": edition.name,
                        "variant": "codex", "entrypoint": "bin/codex",
                        "resourcesDir": "codex-resources", "pathDir": "codex-path"}
            if any(metadata.get(key) != value for key, value in expected.items()):
                continue
            if manifest.get("schema") != "mneme.codex-install.v1":
                continue
            release = manifest.get("release")
            if not isinstance(release, dict) or release.get("tag_name") != "rust-v" + edition.name:
                continue
            executable = (edition / "bin/codex").resolve(strict=True)
            if not executable.is_relative_to(edition) or not executable.is_file() or not os.access(executable, os.X_OK):
                continue
            if not all((edition / name).is_dir() and (edition / name).resolve().is_relative_to(edition)
                       for name in ("codex-resources", "codex-path")):
                continue
            return executable
        except (OSError, ValueError, RecursionError):
            continue
    raise SelectionError("no complete Codex package is available")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--packages", required=True, type=Path)
    parser.add_argument("arguments", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    if not args.arguments or args.arguments[0] != "--":
        parser.error("separate Codex arguments with --")
    try:
        executable = select_package(args.packages)
        os.execvpe(str(executable), [str(executable), *args.arguments[1:]], os.environ)
    except (OSError, ValueError) as exc:
        print(f"Codex launch refused: {exc}", file=sys.stderr)
        return 1
    return 1


if __name__ == "__main__":
    raise SystemExit(main())

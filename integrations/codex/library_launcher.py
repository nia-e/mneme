#!/usr/bin/env python3
"""Select a Codex library before execing its stdio MCP host.

This is the global registration's entry point. A direct native registration
would bypass the project profile boundary entirely.
"""

import argparse
import os
from pathlib import Path
import sys

sys.dont_write_bytecode = True

from profile import select_for_cwd


def _absolute(value, label):
    if (not isinstance(value, str) or not value or "\x00" in value
            or len(value.encode("utf-8")) > 4096 or not Path(value).is_absolute()):
        raise ValueError(label + " must be a bounded absolute path")
    return Path(value)


def select_library(cwd, default_library_config):
    """Resolve the project profile before touching either library config."""
    selection = select_for_cwd(cwd)
    default = _absolute(default_library_config, "default library config")
    chosen = selection["library_config"]
    if chosen is None:
        if selection["mode"] == "isolated":
            raise ValueError("isolated project needs an independent explicit library_config")
        chosen = default
    elif selection["mode"] == "isolated":
        # Symlinks cannot rename the personal library into an isolated one.
        chosen = chosen.resolve(strict=True)
        if chosen == default.resolve():
            raise ValueError("isolated project selected the personal default library")
        if default.exists() and chosen.samefile(default):
            raise ValueError("isolated project selected the personal default library")
    return chosen


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--default-library-config", required=True)
    args = parser.parse_args(argv)
    try:
        # Selection is deliberately before binary/config validation or exec.
        chosen = select_library(Path.cwd(), args.default_library_config)
        binary = _absolute(args.binary, "native MCP binary")
        if binary.is_symlink() or not binary.is_file() or not os.access(binary, os.X_OK):
            raise ValueError("native MCP binary is not an executable regular file")
        if not chosen.is_file():
            raise ValueError("selected library config is not a regular file")
        os.execv(str(binary), [str(binary), "--library-config", str(chosen)])
    except (OSError, ValueError, RuntimeError) as error:
        print("Mneme library selection refused: %s" % error, file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

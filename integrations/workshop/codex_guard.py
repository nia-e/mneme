#!/usr/bin/env python3
"""Cooperative, foreground-only Codex admission for a Pi workshop.

The runner and manual launcher must use the same permanent lock inode. Never
unlink, replace, or "clean up" .pi-codex-activity.lock. This is coordination
between participating launchers, not a process detector or a security boundary:
direct raw-binary, daemon, remote-server and forked-descendant activity is not
covered. A raw executable must preserve the inherited descriptor for the life of
its session; verify that property when upgrading the installed Codex binary.

CLI: codex_guard.py --root /absolute/workshop --codex /absolute/raw/codex -- ARGS
Only direct help/version and auth-management invocations bypass the session gate.
Other invocations conservatively acquire it and add --no-daemon. The process is
then replaced with Codex, preserving the terminal, stdio, exit code and signals.
"""

from __future__ import annotations

import argparse
import fcntl
import os
from pathlib import Path
import stat
import sys
from typing import BinaryIO


LOCK_NAME = ".pi-codex-activity.lock"
BUSY_EXIT = 75


class Refusal(ValueError):
    """The activity gate could not safely be admitted."""


def try_activity_gate(path: Path) -> BinaryIO | None:
    """Return an exclusively held file, or None on contention, without waiting.

    Closing the file releases this reference to the flock. Callers that spawn a
    foreground child should also pass its fd with pass_fds, and retain their own
    reference through child termination. The inode is never removed or truncated.
    Parent-directory aliases resolve to one identity; the final component must
    be a singly linked regular file, never a symlink, directory, device or FIFO.
    """
    return _activity_gate(path, create=True)


def activity_busy(path: Path) -> bool:
    """Inspect cooperative contention without creating or writing a lock file."""
    try:
        gate = _activity_gate(path, create=False)
    except FileNotFoundError:
        return False
    if gate is None:
        return True
    gate.close()
    return False


def _activity_gate(path: Path, *, create: bool) -> BinaryIO | None:
    if not path.is_absolute():
        raise Refusal("activity gate path must be absolute")
    path = path.parent.resolve(strict=True) / path.name
    flags = os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK
    fd = os.open(path, flags | (os.O_CREAT if create else 0), 0o600)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            raise Refusal("activity gate must be a singly linked regular file")
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            os.close(fd)
            return None
        named = path.lstat()
        held = os.fstat(fd)
        if (named.st_dev, named.st_ino) != (held.st_dev, held.st_ino) or held.st_nlink != 1:
            raise Refusal("activity gate identity changed; stop launchers and inspect the lock")
        gate = os.fdopen(fd, "rb+")
    except BaseException:
        os.close(fd)
        raise
    return gate


def session_arguments(arguments: list[str]) -> list[str] | None:
    """None denotes narrow non-session commands; otherwise force foreground.

    This deliberately is not a second Codex argument parser. Metadata/auth calls
    with preceding global options are conservatively gated. All original tokens
    retain their order and exact contents, including tokens following `--`.
    """
    if arguments and arguments[0] in {"--help", "-h", "--version", "-V", "help", "login", "logout"}:
        return None
    option_prefix = arguments[:arguments.index("--")] if "--" in arguments else arguments
    return list(arguments) if "--no-daemon" in option_prefix else ["--no-daemon", *arguments]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, type=Path, help="existing absolute workshop directory")
    parser.add_argument("--codex", required=True, type=Path, help="absolute raw Codex executable, not this shim")
    parser.add_argument("arguments", nargs=argparse.REMAINDER, help="-- followed by unchanged Codex arguments")
    args = parser.parse_args(argv)
    if not args.arguments or args.arguments[0] != "--":
        parser.error("separate Codex arguments with -- (bare -- starts an interactive session)")
    if not args.root.is_absolute() or not args.root.is_dir():
        parser.error("--root must name an existing absolute directory")
    if not args.codex.is_absolute() or not args.codex.is_file() or not os.access(args.codex, os.X_OK):
        parser.error("--codex must name an absolute executable raw Codex binary")
    original = args.arguments[1:]
    arguments = session_arguments(original)
    gate = None
    try:
        if arguments is not None:
            gate = try_activity_gate(args.root / LOCK_NAME)
            if gate is None:
                print("Codex busy: another Pi session holds the workshop activity gate; try again after it finishes.", file=sys.stderr)
                return BUSY_EXIT
            # exec replaces this process: there is no supervisor whose death
            # could release a lock while leaving its direct Codex child running.
            os.set_inheritable(gate.fileno(), True)
        else:
            arguments = original
        os.execvpe(str(args.codex), [str(args.codex), *arguments], os.environ)
    except (OSError, Refusal) as exc:
        print(f"Codex launch refused: {exc}", file=sys.stderr)
        return 1
    finally:
        # Executed only on refusal/exec failure. On exec success the kernel-held
        # descriptor belongs to Codex and is released when its last holder exits.
        if gate is not None:
            gate.close()
    return 1  # exec never returns on success


if __name__ == "__main__":
    raise SystemExit(main())

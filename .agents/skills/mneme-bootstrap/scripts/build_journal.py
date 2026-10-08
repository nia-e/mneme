#!/usr/bin/env python3
"""Refuse offline recovery-journal creation until native bootstrap exists."""

from __future__ import annotations

import argparse
from pathlib import Path
import sys

from journal_contract import JournalError


NATIVE_RECEIPT_REQUIRED = (
    "offline journal construction is disabled: use the future native bootstrap-create "
    "operation, which must produce an authenticated, non-user-forgeable receipt"
)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default=".")
    parser.add_argument("plan", type=Path)
    parser.add_argument("approval", type=Path)
    parser.parse_args()
    raise JournalError(NATIVE_RECEIPT_REQUIRED)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except JournalError as error:
        print(f"build_journal.py: {error}", file=sys.stderr)
        raise SystemExit(2) from error

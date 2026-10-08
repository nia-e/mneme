#!/usr/bin/env python3
"""Refuse offline manifest construction until native bootstrap exists."""

from __future__ import annotations

import argparse
from pathlib import Path
import sys

from journal_contract import JournalError


NATIVE_RECEIPT_REQUIRED = (
    "offline manifest construction is disabled: caller-edited journal state is not "
    "live database evidence; use the future native bootstrap-create operation"
)


def build_manifest(plan: dict, journal: dict) -> dict:
    del plan, journal
    raise JournalError(NATIVE_RECEIPT_REQUIRED)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default=".")
    parser.add_argument("plan", type=Path)
    parser.add_argument("journal", type=Path)
    parser.parse_args()
    raise JournalError(NATIVE_RECEIPT_REQUIRED)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except JournalError as error:
        print(f"build_manifest.py: {error}", file=sys.stderr)
        raise SystemExit(2) from error

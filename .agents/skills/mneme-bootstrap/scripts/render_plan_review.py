#!/usr/bin/env python3
"""Render the exact bootstrap material diff a human must approve."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys

from plan_contract import HARD_MAXIMA
from review_contract import ReviewError, render_review, rendered_sha256


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("plan", type=Path)
    parser.add_argument("--sha256", action="store_true")
    args = parser.parse_args()
    try:
        raw = args.plan.read_bytes()
        if len(raw) > HARD_MAXIMA["plan_bytes"]:
            raise ReviewError("plan exceeds the hard 2 MiB contract limit")
        plan = json.loads(raw.decode("utf-8"))
        if args.sha256:
            print(rendered_sha256(plan))
        else:
            sys.stdout.buffer.write(render_review(plan))
    except (OSError, UnicodeError, json.JSONDecodeError, KeyError, TypeError, ValueError) as error:
        raise ReviewError(f"cannot render review: {error}") from error
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except ReviewError as error:
        print(f"render_plan_review.py: {error}", file=sys.stderr)
        raise SystemExit(2) from error

#!/usr/bin/env python3
"""Validate a recovery journal against its plan and current repository cut."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys

from build_manifest import build_manifest
from journal_contract import JournalError, validate_journal
from manifest_contract import ManifestContractError, parse_manifest
from plan_contract import HARD_MAXIMA, canonical_sha256
from secret_scan import summary as secret_summary
from validate_plan import PlanError, validate


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default=".")
    parser.add_argument("plan", type=Path)
    parser.add_argument("journal", type=Path)
    args = parser.parse_args()
    try:
        root = Path(args.root).resolve()
        plan_raw = args.plan.read_bytes()
        journal_raw = args.journal.read_bytes()
        if (
            len(plan_raw) > HARD_MAXIMA["plan_bytes"]
            or len(journal_raw) > HARD_MAXIMA["journal_bytes"]
        ):
            raise JournalError("plan or journal exceeds the hard 2 MiB contract limit")
        plan = json.loads(plan_raw.decode("utf-8"))
        journal = json.loads(journal_raw.decode("utf-8"))
        for label, raw in (("plan", plan_raw), ("journal", journal_raw)):
            detected_secret = secret_summary(raw)
            if detected_secret is not None:
                raise JournalError(
                    f"{label} contains high-confidence secret material ({detected_secret})"
                )
        terminal_publication_state = (
            isinstance(journal, dict)
            and journal.get("state") in {"verified", "manifest_published"}
        )
        validate(
            plan,
            root,
            raw_size=len(plan_raw),
            allow_greenfield_postcondition=terminal_publication_state,
        )
        result = validate_journal(plan, journal)
        manifest_path = (root / plan["base"]["manifest_path"]).resolve(strict=False)
        if manifest_path.exists():
            if journal["state"] not in {"verified", "manifest_published"}:
                raise JournalError("manifest exists before the journal reached verified state")
            status, actual_manifest = parse_manifest(manifest_path, root)
            if status != "valid":
                raise JournalError("published manifest is absent")
            expected_manifest = build_manifest(plan, journal)
            if actual_manifest != expected_manifest:
                raise JournalError(
                    "published manifest is not the deterministic plan/journal postcondition"
                )
            manifest_hash = canonical_sha256(actual_manifest)
            if journal["state"] == "manifest_published":
                if journal["published_manifest_hash"] != manifest_hash:
                    raise JournalError("published manifest hash disagrees with journal")
                result["published_manifest_hash_verified"] = manifest_hash
            else:
                result["publication_recovery"] = {
                    "detected_manifest_hash": manifest_hash,
                    "required_journal_state": "manifest_published",
                }
        elif journal["state"] == "manifest_published":
            raise JournalError("journal claims publication but the manifest is absent")
    except (
        OSError,
        UnicodeError,
        json.JSONDecodeError,
        PlanError,
        ManifestContractError,
        KeyError,
        TypeError,
        ValueError,
    ) as error:
        raise JournalError(f"cannot validate journal: {error}") from error
    json.dump(result, sys.stdout, ensure_ascii=True, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except JournalError as error:
        print(f"validate_journal.py: {error}", file=sys.stderr)
        raise SystemExit(2) from error

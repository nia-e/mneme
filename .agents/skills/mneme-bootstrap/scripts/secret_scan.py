#!/usr/bin/env python3
"""Small deterministic high-confidence secret scanner for bootstrap artifacts.

This is a fail-closed eligibility rail, not a claim of comprehensive detection.
Findings report only pattern labels and line numbers so diagnostics do not echo the
suspected secret.
"""

from __future__ import annotations

import re


KNOWN_PATTERNS = (
    ("private-key", re.compile(r"-----BEGIN (?:RSA |EC |OPENSSH |DSA )?PRIVATE KEY-----")),
    ("aws-access-key", re.compile(r"(?<![A-Z0-9])(?:AKIA|ASIA)[A-Z0-9]{16}(?![A-Z0-9])")),
    ("github-token", re.compile(r"(?<![A-Za-z0-9_])(?:gh[pousr]_[A-Za-z0-9]{30,255}|github_pat_[A-Za-z0-9_]{40,255})(?![A-Za-z0-9_])")),
    ("slack-token", re.compile(r"(?<![A-Za-z0-9-])xox[baprs]-[A-Za-z0-9-]{20,255}(?![A-Za-z0-9-])")),
    ("openai-like-key", re.compile(r"(?<![A-Za-z0-9-])sk-[A-Za-z0-9_-]{32,255}(?![A-Za-z0-9_-])")),
)

ASSIGNMENT = re.compile(
    r"(?i)[\"']?(?:password|passwd|api[_-]?key|access[_-]?key|client[_-]?secret|auth[_-]?token)[\"']?"
    r"\s*[=:]\s*[\"']?([^\s\"'#;,]{20,512})"
)
PLACEHOLDERS = {
    "example",
    "fake",
    "not-real",
    "placeholder",
    "redacted",
    "replace-me",
    "sample",
    "test",
    "xxxxx",
}


def looks_high_entropy(value: str) -> bool:
    lowered = value.lower()
    if lowered in PLACEHOLDERS:
        return False
    classes = sum(
        (
            any(character.islower() for character in value),
            any(character.isupper() for character in value),
            any(character.isdigit() for character in value),
            any(not character.isalnum() for character in value),
        )
    )
    return len(set(value)) >= 12 and classes >= 3


def findings(data: bytes) -> list[dict[str, object]]:
    try:
        text = data.decode("utf-8", "strict")
    except UnicodeDecodeError:
        return [{"kind": "non-utf8", "line": 0}]
    found: set[tuple[str, int]] = set()
    for line_number, line in enumerate(text.splitlines(), 1):
        for label, pattern in KNOWN_PATTERNS:
            if pattern.search(line):
                found.add((label, line_number))
        for match in ASSIGNMENT.finditer(line):
            if looks_high_entropy(match.group(1)):
                found.add(("credential-assignment", line_number))
    return [
        {"kind": kind, "line": line}
        for kind, line in sorted(found, key=lambda item: (item[1], item[0]))
    ]


def summary(data: bytes) -> str | None:
    detected = findings(data)
    if not detected:
        return None
    return ", ".join(f"{item['kind']}@{item['line']}" for item in detected)

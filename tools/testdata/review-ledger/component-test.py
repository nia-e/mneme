import os
from pathlib import Path


_sentinel = os.environ.get("REVIEW_LEDGER_EXECUTION_SENTINEL")
if _sentinel is not None:
    Path(_sentinel).write_text("evidence was executed\n", encoding="utf-8")
raise RuntimeError("review-ledger evidence was executed")


def test_rejects_unsafe_widget():
    """The committed component-test locator; the verifier must not execute it."""
    raise RuntimeError("review-ledger evidence was executed")

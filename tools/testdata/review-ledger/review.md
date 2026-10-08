# Independent widget review

The implementation was reviewed at the commit recorded separately in the ledger.

### F1 — Unsafe widget input is accepted

The parser admits the sentinel value and needs a focused regression test.

### F2 — Lease release remains synchronous

The behavior is accepted for now but needs an explicitly owned follow-up.

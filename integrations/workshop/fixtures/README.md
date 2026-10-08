# Representative migration fixtures

`signal_store_v1.py` and `signal_store_v2.py` preserve the historical ledger
schemas and reader/writer behavior used by upgrade tests. They are sanitized
source fixtures: owner labels are neutral, and they are **not byte-identical
copies of deployed packages**. Tests create disposable synthetic ledgers with
these readers, migrate their rows and check that prior writers refuse the new
format. No account data or conversation history is included.

`wake_v3.py` preserves the prior queue implementation for the same purpose.
Queue tests create synthetic state and check that the prior writer refuses a
queue upgraded to the current format.

## Integrity

Keep the fixture schemas and migration coverage intact. If representative
source changes deliberately, update its recorded hash and test expectation;
do not claim identity with a historical deployed artifact.

```text
signal_store_v1.py  d86a8ea0eface11271ca9b31abaead8d0ec64b4c1ae563f7382c61e1d75f7efd
signal_store_v2.py  318f235248f1b1809876afaaa9693a6bf6f7b9bb5ff21e19edbffd1f28655c76
wake_v3.py  df38ecd86e624499955bcdfdd6921f2fc98d1c3d1351794052e5960a153e825b
```

# Recall-selection v1 authored fixtures

This is a fixed, authored evaluation fixture, **not** an efficacy study or a sample of production recall traffic. All notes and situations are invented, public-safe, and written as trusted but messy agent/project experience. They contain no malicious instructions. The dataset tests one-shot selection of complementary evidence under a byte budget, not whether retrieval found the notes in the first place.

`dev.inputs.json` and `test.inputs.json` contain only policy-visible queries, budgets, and candidate notes. The separate `*.assessment.json` files contain scorer-only exact-text requirements. The held-out assessment must not be read by a policy implementer while developing the selector. That separation is an evaluation convention, **not filesystem secrecy**.

Each split is balanced across four families and two byte budgets. Every case has 16 notes: 14 active and two probationary candidates. A few source notes put a decisive fact after a longer, natural handoff preface, so selecting the right ID alone is insufficient if budget packing truncates its emitted text. Requirements identify exact source-note substrings; they are not semantic paraphrase grading. Scoped-correction cases require both the newer instruction and still-applicable legacy scope, rather than “latest wins.” Four held-out cases intentionally lack one required fact; their absent `node_key` is not present in the corpus and must not be injected as gold.

| Split | Cases | Per family | 4096 B | 8192 B | Missing-evidence cases |
| --- | ---: | ---: | ---: | ---: | ---: |
| Development | 8 | 2 | 4 | 4 | 0 |
| Held-out | 24 | 6 | 12 | 12 | 4 |

The authored notes were fixed before selection-policy runs. Treat later changes
as a new fixture version rather than tuning against held-out outcomes.

---
name: mneme-adversarial
description: >-
  Search mneme memory for counter-evidence, prior failures, pitfalls, and reasons a
  proposed decision may be wrong. Use when the user explicitly requests a red-team,
  objections, failure history, or "what could go wrong" analysis, or before a
  consequential decision that is irreversible, externally published, security- or
  safety-sensitive, or costly to unwind. Do not invoke for routine, local, reversible
  plans or every claim. The search is read-only; contradiction flags, reflection,
  and new memories require separate explicit memory-write authority. Use the focused
  mneme-recall walk instructions; consult the full `mneme` hub only for missing exact
  mechanics.
---

# mneme-adversarial — retrieve counter-evidence

Run a second, adversarial retrieval only for an explicit red-team request or a
consequential decision that is irreversible, public, security- or safety-sensitive,
or expensive to unwind. Routine implementation choices and reversible local plans do
not qualify. Same graph as recall, opposite intent. Use mneme-recall for the walk
mechanics; do not load the full hub unless an exact lower-level detail is absent from
the focused cards.

## 1. Seed from your pitfalls, scoped to the decision

`tags` restricts the semantic *seeds* to `pitfall` nodes; tagged retrieval reports
its bounded coverage. The walk then spreads out —
often straight onto the thing you're considering, which is exactly the link you want
to see:

```
query{db, text:"<the approach/claim you're about to commit to>", tags:["pitfall"]}
```
CLI: `mnemed query "…" --tag pitfall`. Also run a **counter-phrased** query (no tag)
to catch failure-shaped memories that were never tagged:
`query{db, text:"why <X> fails / <X> gotchas / problems with <X>"}`. Search only
the selected owners authorized for this decision. A project-only question does
not require or authorize private global recall.

## 2. Walk for objections

Walk those seeds exactly as in **mneme-recall** (read-only — gather counter-evidence,
do not train the graph), but with adversarial intent: *"find anything suggesting this
approach is wrong, risky, or has failed before."* Drive the walk locally by default;
use sub-agents only under mneme-recall's host-permission and justification rules.
Finish with `abort` unless explicit memory-write authority permits reflection; only
then use `done` and retain the receipt.

## 3. Decide

Surface the objections, then either **address them** or **proceed** while noting that
an empty result is bounded evidence, not proof of safety.

Do not mutate memory by default. Only with explicit memory-write authority may you:

- call `reflect` using a receipt under mneme-recall's grounded-training rules;
- flag a real conflict with `contradict{db, a, b}` for **mneme-reconcile**; or
- invoke **mneme-remember** to capture a new durable lesson, optionally tagged
  `pitfall`.

Without that authority, report the candidate contradiction or lesson in the answer
and leave the graph unchanged.

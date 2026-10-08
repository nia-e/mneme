# MCP database maintenance handoff

Use this handoff to run offline `mnemed` maintenance while an MCP server is
running. It releases one database, not the whole server. Ordinary tools for that
database remain unavailable until you resume it.

The MCP process must have started with the trusted `operator` profile.
`database_control status` works in every profile, but `release` and `resume` are
operator-only. Profiles cannot be elevated at runtime: if a curator owns the
store, stop it before running the one-shot CLI, or restart it as operator first.
A second operator cannot open a store whose lease is still held.

1. Call `database_control` with `{"db":"project","action":"status"}`.
2. Finish or abort every live walk. Reflect, or let expire, every unconsumed receipt
   for that database; a reflect currently in progress must also finish.
3. Call `database_control` with `{"db":"project","action":"release"}`.
4. Run the one-shot `mnemed` maintenance command. Its ordinary lease acquisition is
   the proof that MCP really released the database.
5. Call `database_control` with `{"db":"project","action":"resume"}`.

If release is blocked, finish the reported work before retrying. A live walk
retains traversal state; an unconsumed or claimed receipt retains feedback
permission; active requests and detached backend jobs may still use the store.
Cancelling an async request does not necessarily stop its blocking backend job.

Release checkpoints snapshot stores, closes the handle and lease, and rotates
the selected database's feedback authority epoch (its retry-identity generation).
Consumed-receipt tombstones—the records used to recognize successful retries—are
then purged because their old authority is no longer valid. Unconsumed or claimed
receipts are not purged and remain blockers.

Resume re-resolves the configured generation selector and opens the potentially
updated generation. If the CLI or another process still holds the lease, resume
fails and the maintenance fence stays in place. Normal tools fail closed while
the database is fenced.

## Why this is explicit, not an idle timer

Transport idleness is not database quiescence. A client can be silent while the
server retains a resumable walk, an unconsumed feedback receipt, or a reflect claim
whose storage outcome may still be ambiguous. Reopening a persistent store
deliberately clears old host-local retry proofs. Evicting on a timer could therefore
invalidate a capability that still appears live or, worse, let a retry apply
feedback twice under reused authority.

The exclusive process lease prevents competing Mneme engines from overwriting
multi-step learning updates; SQLite locking alone does not protect those updates.
Before releasing that lease, the handoff checks three kinds of retained state:

- every operation owns an `Arc` containing both the database handle and its OS lease;
  release succeeds only when the registry owns the sole reference;
- every detached Cozo blocking job owns a separate activity token inside the blocking
  closure, so request cancellation cannot make the backend look quiescent early;
- release holds the session-state lock while proving that the selected database has
  no active walk or unconsumed/claimed receipt and installing the checkout fence;
  its cancellation-safe worker later reacquires that state to rotate only the
  selected database's feedback epoch before deleting consumed retry tombstones.

Remote-edge hydration uses the same identity discipline: matching a stable database
id and cloning its open handle happen under one slot lock. If offline maintenance
publishes a new generation with a different id, an edge to the old generation stays
unresolved instead of being queried against the replacement database.

A wall-clock timeout proves none of these lifetimes have ended.

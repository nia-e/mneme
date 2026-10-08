---
name: mneme-bootstrap-inspect
description: >-
  Inspect and interpret a repository's Mneme bootstrap state using only native
  `mnemed --json bootstrap-inspect`. Use for read-only status, mode-selection, or
  blocker questions about an absent, managed, conventional-unmanaged, interrupted,
  malformed, orphaned, or symlinked project store. This skill must never write files,
  create or open a database, load an embedder, acquire or release a lease, repair
  state, or produce bootstrap artifacts; use mneme-bootstrap for requested plans,
  proposals, or native creation.
---

# Inspect Mneme bootstrap state

Use the already-installed `mnemed` binary from the target Git repository root:

```sh
mnemed --json bootstrap-inspect --root .
```

Keep stdout in the conversation; do not redirect it. If the binary is missing or the
command cannot run read-only, report that blocker. Do not build, install, migrate,
load another skill, or fall back to another Mneme command.

The inspector is the sole mode-selection probe. It does not create `.mneme`, enter the
normal database resolver/open path, load an embedder, or create a lease inode. It may
open an already-present lease inode read-only for a nonblocking shared observation;
that observation is advisory. Every result is `authoritative:false` and
`retry_safe:true`; a later native mutation must reacquire its own exclusive lease and
recheck live state.

Interpret `state` exactly:

- `greenfield_absent`: no project store is present; report the suggested mode and any
  fresh suggested identifiers, but do not create directories or artifacts.
- `active_managed`: bounded authenticated artifacts identify an active managed
  generation; report identity, generation, lease observation, and capability.
- `conventional_unmanaged`: a conventional database exists without trustworthy
  managed identity; identity may be unknown because inspection must not open Cozo.
- `interrupted_activation`: only an exact native retry with the original reviewed
  plan, approval, and operation id may recover it. Do not select or delete anything.
- `blocked_malformed`, `blocked_orphan`, or `blocked_symlink`: fail closed. Report the
  returned blocker and action verbatim; do not repair, bypass, follow symlinks, choose
  a generation by recency, or fall back to conventional open.

A `held` lease changes mutation capability, not the layout classification. Report it;
never release, resume, replace, or acquire a lease. Report the schema version/namespace,
repository root, state, recommended mode, apply capability, safely exposed identity,
lease observation, blocker/action, and the non-authoritative limitation. Make no
filesystem, database, embedder, or lease changes—full stop.

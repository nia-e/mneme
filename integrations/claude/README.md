# Claude Code capture adapter

This optional adapter records selected, source-bound Claude Code observations through
Mneme's **shared recorder**. It does not install Codex, read Codex authentication,
start a memory owner, create a store, or configure semantic recall/startup core
memory. Keep any existing external recall/core wiring unchanged.

**Status:** source tests, isolated copied-helper smoke, and offline metadata
inspection of an integrity-checked official Claude artifact. Synthetic transcript,
hook and executable fixtures exercise the adapter; they do **not** establish
that an installed Claude Code version delivers these hooks or honors the runtime
flags. No Claude version has been validated end to end here.
There is no automatic Claude installation command. In particular, `mnemed init`
remains the Codex setup path, not a Claude setup shortcut.

## Prepare, review, enable

Prerequisites: Python **3.11+**, matching native `mnemed` with persistent storage
and HTTP support, an already configured native memory selector/owner, and an
explicit Claude Code executable authenticated through its ordinary subscription
setup. See [declarative configuration](../../docs/declarative-config.md) for
native owner selection, activation and recording grants. This adapter does not
supply a second selector or change those grants.

Keep the helper checkout at a stable absolute path. The flat Python runtime lives
under `integrations/codex` for historical packaging reasons; Claude does not use a
Codex executable, configuration or credentials. A copied package must retain the
complete runtime inventory, not just the `claude_*` modules.

From the Mneme checkout, prepare a review artifact (replace every example path
and select the full model ID deliberately):

```sh
python3.11 integrations/codex/claude_config.py \
  --memory-config /path/to/memory.toml \
  --native-cli /path/to/mnemed \
  --state-dir /path/to/private-state/claude-capture \
  --reader-claude /path/to/claude \
  --reader-model claude-sonnet-4-5-20250929 \
  --librarian-effort medium \
  --transcripts-root /path/to/explicit/transcript-directory \
  --hooks-program /path/to/mneme/integrations/codex/claude_hooks.py \
  --hook-config-path /path/to/private-config/claude-capture.json \
  > /path/to/review/claude-capture-prepare.json
```

Preparation prints JSON and does not create configuration/state directories,
contact owners, run the supplied executables, install hooks, or read authentication.
Review its `hook_config` (including executable SHA256 values, native selector,
transcript root, full model ID and dedicated state directory) and
`claude_settings.hooks` command paths. Paths must be absolute; model aliases such
as `sonnet` and `claude-sonnet` are rejected.

After review, create the private configuration directory yourself, then extract
only the reviewed adapter configuration:

```sh
umask 077
python3.11 - /path/to/review/claude-capture-prepare.json <<'PY'
import json, pathlib, sys
prepared = json.loads(pathlib.Path(sys.argv[1]).read_text())
target = pathlib.Path(prepared["hook_config_path"])
with target.open("x") as output:
    json.dump(prepared["hook_config"], output, indent=2)
    output.write("\n")
PY
```

Manually merge the reviewed `claude_settings.hooks` into the intended Claude
project's settings. Preserve existing handlers/settings; do not replace the
whole file with this snippet. It registers `SessionStart`, `UserPromptSubmit`,
`Stop`, `StopFailure`, `SessionEnd` and `PreCompact`. Start a **fresh session**.
There is no trust bypass, global-config update, daemon setup or automatic hook
activation in this helper. To disable it, remove only these reviewed handlers;
retain state/ledgers for inspection rather than resetting paid-work accounting.

## What is supported

- Root sessions, including root `--agent` sessions. Actual child `agent_id` and
  sidechain/subagent transcripts are excluded. Resume may recover retained,
  unpaid Stop candidates; it never renews an observing deadline or reopens paid
  work. Fork/clear/compaction fence unclosed work, not a fresh paid-work allowance.
- Exact hook `prompt_id` plus prompt bytes bind a source observation. `prompt_id`
  is not invented as a transcript field. `Stop` keeps only a provisional latest
  response receipt: it does not launch inference. The next root prompt,
  `SessionEnd`, or legitimate resume requests one bounded snapshot per prompt.
  The worker makes at most three flush polls within five seconds, preferring a
  matched latest response and complete tool exchanges; after that allowance it
  may assess a valid complete-line prefix. Unmatched tool calls and a trailing
  partial line are omitted and reported. Malformed complete records, changed
  anchors, branch changes and unknown formats still defer capture.
- A snapshot freezes exact physical source references, a digest of the observed
  byte range, latest-Stop match status and omissions before assessment. Coverage
  is **observed prefix; turn finality unproven; remaining transcript unknown**.
  Neither a later prompt, `SessionEnd`, EOF, an unchanged size nor assistant
  confidence establishes completion or task success. Later writes cannot expand
  an admitted snapshot. Capture of the final turn is still not guaranteed.
- Claude snapshot recording ledgers use v2; v1 readers must refuse them. Current
  readers also refuse legacy v1 recording ledgers instead of implicitly migrating
  paid-work history. Retain old state for inspection; do not reset accounting.
  Pending legacy v1 Claude touchstone nominations remain read-only consumable
  under their original closed-source coverage; new nominations use v2 snapshot
  coverage. Existing Codex formats and state remain unchanged.
- Native selection determines eligible owners and capture policy. Disabled or
  unavailable recording stays disabled/unavailable; there is no fallback to a
  private global store. Preparation does not grant writes. Capture-only mode
  leaves semantic recall and background maintenance off.
- One shared, bounded assessment policy and recorder validate citations, exact
  source fields, proposals and replay identity. Claude source/job identities remain
  distinct from existing Codex jobs. Existing Codex receipts and ledgers are not
  upgraded or reset implicitly.

The assessor hashes the explicit executable, probes bounded `--version`/`--help`,
and refuses unsupported runtimes before an assessment attempt. The metadata
compatibility check requires Claude Code major version 2, at least 2.1.259, and
advertised required switches, except the documented but hidden `--max-turns`.
That exception does not remove `--max-turns=1` from the runtime command. Metadata
checks remain conservative; passing `--help`/`--version` is not proof that the
assessment switches are accepted. It uses a fresh
private neutral working directory, one model call/turn, no ordinary tools, no MCP,
no session persistence, explicit settings isolation, and disabled built-in agents.
Reported loaded plugin/skill/agent metadata is refused. Strict JSON validation is
host-side: **no `--json-schema` repair turns or retry with weaker flags**.

Safe mode preserves normal subscription authentication; it is **not an OS
sandbox**. Managed Claude policy can still execute commands. The adapter removes
inherited API/provider billing switches instead of silently using an inherited
API key. That environment policy is not proof of actual subscription billing.
Do not use it where a managed-policy-free process is required.

Whole-call model usage includes uncached input, cache reads and cache writes;
reasoning usage is unknown, not free. Missing/ambiguous usage halts paid work
without automatic retry. Receipts omit raw reasoning, prompts, stderr and
credentials. Selected public transcript evidence can still reach the configured
provider: enabling capture is not a promise that material remains on-device.

## Meaning is not approval

A nomination from the background recorder is not a saved touchstone. The
foreground agent reviews and, when justified, explicitly approves the nomination;
it need not ask the user to approve the agent's authored meaning. There is no
nomination quota. See [touchstones](../../docs/touchstones.md).

A source-derived `created` value is a full epoch-millisecond instant, separate
from when evidence was verified or delivered. It does not turn an incomplete
source turn into a completed one. See [episodes](../../docs/episodic-memory.md).

## Validation boundary

Tests cover documented-shaped synthetic transcript/hook data and fake headless
processes, including unsupported capabilities, changed executable bytes, bounded
output, missing usage and cleanup. Copied-helper smoke tests use a private
throwaway home and no Codex executable on `PATH`; native bundle tests verify
inventory and extracted bytes. The copied-process smoke is POSIX-only; it does
not establish native Windows end-to-end behavior. These are package/adapter checks, **not** live
Claude, subscription billing, real owner/store, managed-policy or recall-quality
validation. Review those boundaries before enabling this on real sessions.

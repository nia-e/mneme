# Claude Code capture adapter

This optional adapter records selected, source-bound Claude Code observations through
Mneme's **shared recorder**. It does not install Codex, read Codex authentication,
start a memory owner, create a store, or configure semantic recall/startup core
memory. Keep any existing external recall/core wiring unchanged.

**Status:** source tests and isolated copied-helper smoke cover the adapter.
A local Claude Code **2.1.308** no-inference check on 2026-10-25 also observed real
`SessionStart`, `UserPromptSubmit` (including `prompt_id`) and `SessionEnd` payloads,
and exercised a generated hook command's nonfatal deferral for absent configuration.
That establishes hook plumbing, not full recording or assessment compatibility:
no Claude version has been validated end to end here.
There is no automatic Claude installation command. In particular, `mnemed init`
remains the Codex setup path, not a Claude setup shortcut.

## Prepare, review, enable

Prerequisites: Python **3.11+**, matching native `mnemed` with persistent storage
and HTTP support, an already configured native memory selector/owner, and Claude
Code authenticated through its ordinary subscription setup and available on the
hook process's `PATH` (or selected by an explicit absolute path).
See [declarative configuration](../../docs/declarative-config.md) for
native owner selection, activation and recording grants. This adapter does not
supply a second selector or change those grants.

Keep the helper checkout at a stable absolute path. The flat Python runtime lives
under `integrations/codex` for historical packaging reasons; Claude does not use a
Codex executable, configuration or credentials. A copied package must retain the
complete runtime inventory, not just the `claude_*` modules.

From the Mneme checkout, prepare a review artifact using Python 3.11+ (replace
every example path and select the full model ID deliberately):

```sh
python3 integrations/codex/claude_config.py \
  --memory-config /path/to/memory.toml \
  --state-dir /path/to/private-state/claude-capture \
  --reader-model claude-sonnet-4-5-20250929 \
  --librarian-effort medium \
  --transcripts-root /path/to/explicit/transcript-directory \
  --hooks-program /path/to/mneme/integrations/codex/claude_hooks.py \
  --hook-config-path /path/to/private-config/claude-capture.json \
  > /path/to/review/claude-capture-prepare.json
```

Preparation prints JSON and does not create configuration/state directories,
contact owners, run the supplied executables, install hooks, or read authentication.
Review its `hook_config` (native selector, transcript root, full model ID and
dedicated state directory) and `claude_settings.hooks` commands. Data/configuration
paths must be absolute; model aliases such as `sonnet` and `claude-sonnet` are
rejected.

New adapters use `mnemed` and `claude` from the invoking process's `PATH`; prepare
does not require either executable to be installed yet. `--native-cli` and
`--reader-claude` also accept absolute executable selectors, preserving symlink
spelling. New preparation does not add executable SHA256 pins; existing explicit
pins remain enforced. On POSIX, generated hooks discover compatible Python 3.11+
on `PATH` at each launch; `--python /absolute/selector` chooses an explicit
interpreter instead. A GUI host's environment must expose those commands too:
an interactive terminal's `PATH` does not establish the hook's `PATH`.

After review, create the private configuration directory yourself, then extract
only the reviewed adapter configuration:

```sh
umask 077
python3 - /path/to/review/claude-capture-prepare.json <<'PY'
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

The assessor resolves the selected executable for each call and verifies its
SHA256 only when explicitly configured. It does not gate execution on a version
number or `--help` output. Missing commands, rejected switches and protocol errors
fail the bounded attempt without retrying with weaker flags. It uses a fresh
private neutral working directory, one model call/turn (`--max-turns=1`), no
ordinary tools, no MCP, no session persistence, explicit settings isolation, and
disabled built-in agents.
Claude's safe mode still loads [built-in plugins](https://code.claude.com/docs/en/plugins/mods/overview#mods-built-into-claude-code).
The isolated call settings explicitly disable the bundled AGENTS.md and plugin
authoring plugins; this does not edit the user's settings or allowlist plugin
activity. Unknown or managed customizations remain subject to the same refusal.
Reported loaded plugin/skill/agent metadata is refused. Strict JSON validation is
host-side: **no `--json-schema` repair turns or retry with weaker flags**.

Safe mode preserves normal subscription authentication; it is **not an OS
sandbox**. Managed Claude policy can still execute commands. The adapter removes
inherited API/provider billing switches instead of silently using an inherited
API key. That environment policy is not proof of actual subscription billing.
Do not use it where a managed-policy-free process is required.

Whole-call model usage includes uncached input, cache reads and cache writes;
reasoning usage is unknown, not free. Missing/ambiguous usage halts paid work for
its owning turn-end packet, not the whole session, without automatic retry.
Later packets do not clear that retained uncertainty. Packet accounting leaves
the per-call `CLAUDE_CODE_MAX_OUTPUT_TOKENS` setting unchanged (10k/20k/40k by
effort), along with prompt/answer bounds and deadlines. Receipts omit raw
reasoning, prompts, stderr and credentials. Selected public transcript evidence
can still reach the configured provider: enabling capture is not a promise that
material remains on-device.

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
processes, including runtime failure, dynamic executable replacement, explicit
pin mismatches, bounded output, missing usage and cleanup. Copied-helper smoke
tests use a private throwaway home and no Codex executable on `PATH`; native
bundle tests verify inventory and extracted bytes. The copied-process smoke is
POSIX-only; it does not establish native Windows end-to-end behavior. The local
CLI hook check above blocked the prompt before inference and recorded zero model
turns, tokens and cost. Neither check validates a full Claude recording cycle,
subscription billing, real owner/store behavior, managed policy or recall quality.
Review those boundaries before enabling this on real sessions.

# Private workshop mailbox

The mailbox lets another agent queue a private task for a [workshop](README.md)
and poll for its reply. It is a Python stdio MCP server launched through SSH:
no public listener, push subscription or model process of its own.

Use it to delegate research or implementation to a remote workshop. SSH
account authentication is the trust boundary; a `request_id` is a retry key,
not a verified sender identity. A peer delegation is not the human owner and
grants no new OS privileges or public/external-send permission. Quoted, forwarded
and web material remains context rather than independent authority.

## Connect

Install `mailbox.py` alongside the [workshop runtime](README.md#install) on the
remote host. Configure your MCP client to launch this command with your actual
SSH account, installed script path and workshop root:

```sh
ssh -T user@remote-host \
  python3 /home/user/.local/share/mneme/workshop/current/mailbox.py \
  --root /home/user/workshop
```

Python 3.11+ is required; there are no extra Python packages. Keep the account
and root private. The mailbox has no separate token or per-sender access control;
do not expose it as HTTP or attach it to a public chat platform. It offers only
`send_message`, `read_reply` and `workshop_status`, not shell/filesystem tools.

Install and review the workshop scheduler separately: starting this server does
not enable it. Add `--mailbox-inbox` to the installed runner command for
arrival-driven interactive work. Otherwise messages follow background scheduling.
Signal is a separate opt-in. Interactive inboxes share 48 starts per UTC day and
eight per rolling hour by default; `--unlimited-interactive` explicitly removes
only those start ceilings. Pause, locks, cycle timing and cooldown still apply.
See [scheduling](README.md#opt-in-interactive-inbox-lane).

## Send a task and read its reply

Call these through your MCP client, not as synchronous shell commands:

```text
send_message({"request_id":"research-001","text":"Investigate the failing test and leave a report."})
read_reply({"request_id":"research-001"})
workshop_status({})
```

`send_message` durably queues one request and returns `accepted: true` with its
status. **Accepted means queued, not seen, answered or executed.** Retry with
exactly the same key and text, even after archival; the retained record is
returned without requeueing. Different text with the same key is refused.
Text must be nonblank and 1–4000 characters. IDs are 1–96 ASCII letters, digits,
`_` or `-`, starting with a letter/digit.

`read_reply` returns `reply: null` for unknown, pending, held or unfinished work.
`delivered` only means orientation saw the request. Only a validated final reply
beside a completed workshop receipt—or its immutable archived outcome—is shown.
A completed cycle without a reply reports `completed_without_reply`; deliberate
silence is not a send failure. Poll later: this is not a synchronous chat RPC.

A running cycle can temporarily withhold unarchived replies until it releases
its lock, even if a file appears completed. With unlimited cycle timing that wait
has no fixed upper bound. Archived replies remain readable during another cycle.
New requests queue rather than spawning an overlapping session. Drafts, failed
cycles and corrupt/unknown state never masquerade as answers.

`workshop_status` reports pause, recorded start counts, running receipts and
queued mailbox count. It cannot know the current service flags: configured
cap/due fields are null. `last_observed_cycle` is historical policy evidence;
`stored_relative_next_due_at` is not an hourly schedule. Status reads start no
work and change no state.

## Capacity and operation

The shared queue holds at most 64 records/256 KiB, with at most four events and
four structured replies per cycle. The service's nondeleting maintenance archives
validated completed work and preserves exact retries/replies. A full queue of
unresolved work still refuses new requests; it does not silently prune them.
Archives have no implicit expiry. For manual archival, pause an idle workshop and
follow [the archive procedure](README.md#nondeleting-maintenance-and-operator-archive).
Do not remove rows or reset sequence counters to make room.

The [server](mailbox.py) accepts newline-delimited JSON frames up to 64 KiB and
MCP versions `2025-11-25`, `2025-06-18` and `2024-11-05`; EOF closes it cleanly.
The three-tool catalog is a single page. Standard bounded `_meta` on `tools/list`
and `tools/call` is accepted but never interpreted as tool arguments or authority.
No progress notifications or reply push mechanism exist.

## Verification and limits

From the repository root:

```sh
python3 -m unittest discover -s integrations/workshop -p 'test_mailbox.py' -v
```

Tests use disposable state and fake receipts, not a model or live remote service.
After installation, verify the copied script identity and make a finite
initialize/catalog/send/read/refusal/EOF probe through your actual SSH/MCP client.
Use a test request you intend the workshop to receive; this does not authorize
unsolicited or public messages.

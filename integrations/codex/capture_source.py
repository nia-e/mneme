"""Checked host routing for the single capture lifecycle, not another recorder.

Absent host axes mean the original Codex recipe only. Claude adapters use their
own explicit configuration, evidence and state namespace; source coordinates are
provenance, never per-agent authorization.
"""
from dataclasses import asdict
from pathlib import Path
import os

CODEX = "codex"
CLAUDE = "claude_code"
HOSTS = (CODEX, CLAUDE)
LEGACY_OBSERVATION_SCHEMA = "mneme.capture-turn-observation.v1"
OBSERVATION_SCHEMA = "mneme.capture-turn-observation.v2"
CODEX_CONFIG_SCHEMAS = frozenset(f"mneme.codex-hooks.config.v{version}" for version in range(1, 14))


def source_host(config):
    host = config.get("source_host", CODEX)
    if host not in HOSTS:
        raise ValueError("unsupported_source_host")
    schema = config.get("schema")
    if host == CLAUDE and schema != "mneme.claude-hooks.config.v1":
        raise ValueError("unsupported_capture_config")
    # Schema-less direct callers are offline compatibility/test helpers; both
    # production loaders independently require a complete known host schema.
    if host == CODEX and schema is not None and schema not in CODEX_CONFIG_SCHEMAS:
        raise ValueError("unsupported_capture_config")
    return host


def assessor(config):
    value = config.get("assessor", CODEX)
    if value not in HOSTS:
        raise ValueError("unsupported_assessor")
    return value


def directory(config, lane):
    host = source_host(config)
    base = Path(config["state_dir"])
    return base / lane if host == CODEX else base / "capture" / host / lane


def schema(config, codex_schema, lane):
    if source_host(config) == CODEX:
        return codex_schema
    # Recording v2 fences the old whole-turn observer; reader v2 fences the old
    # lifetime accounting writer. Other lanes retain their existing schemas.
    return f"mneme.claude-{lane}.state.v{2 if lane in ('recording', 'reader') else 1}"


def _observer(config):
    if source_host(config) == CLAUDE:
        import claude_observer
        return claude_observer
    import turn_observer
    return turn_observer


def admit(config, event):
    host = source_host(config)
    root = (Path(config["transcripts_root"]) if host == CLAUDE else
            Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))) / "sessions")
    from rollout_primitives import digest
    return _observer(config).admit_source_turn(event.get("transcript_path"), sessions_root=root,
        session_id=event["session_id"], turn_id=event["turn_id"],
        expected_prompt_sha256=digest(event["prompt"].encode()))


def restore(config, value):
    if source_host(config) == CLAUDE:
        return _observer(config).restore_admission(value)
    from turn_observer import TurnAdmission, RecordAnchor
    return TurnAdmission(**{**value, "session_record": RecordAnchor(**value["session_record"]),
                             "start_record": RecordAnchor(**value["start_record"])})


def observe(config, admission, *, limits, delivery=None, stop_receipt=None, boundary_receipt=None,
            allow_partial=False, snapshot=None):
    kwargs = {"limits": limits, "delivery": delivery}
    if source_host(config) == CLAUDE:
        kwargs["stop_receipt"] = stop_receipt
        kwargs["boundary_receipt"] = boundary_receipt
        kwargs["allow_partial"] = allow_partial
        kwargs["snapshot"] = snapshot
    return _observer(config).observe_source_turn(admission, **kwargs)


def assessable(observation):
    """Observation readiness is not a claim of source/task completion."""
    return (observation.get("status") == "complete"
            and observation.get("reason") == "source_turn_complete"
            or observation.get("schema") == OBSERVATION_SCHEMA
            and observation.get("source_host") == CLAUDE
            and observation.get("status") == "observed"
            and observation.get("reason") == "source_snapshot_ready")


def byte_ceiling(config, admission, *, limits):
    return _observer(config).observation_byte_ceiling(admission, limits=limits)


def pin(config, admission):
    if source_host(config) == CLAUDE:
        return _observer(config).pin(admission)
    value = asdict(admission)
    return {k: value[k] for k in ("path", "sessions_root", "session_id", "device", "inode", "session_record")}


def start_record(config, admission):
    if source_host(config) == CLAUDE:
        return _observer(config).source_start(admission)
    return asdict(admission.start_record)


def start_offset(config, admission):
    if source_host(config) == CLAUDE:
        return admission.start_offset
    return admission.start_record.byte_offset


def first_turn(config, admission):
    if source_host(config) == CLAUDE:
        return _observer(config).first_turn(admission)
    return admission.start_record.byte_offset == admission.session_record.line_bytes


def snapshot_end(config, admission, observation):
    if source_host(config) == CLAUDE:
        return _observer(config).snapshot_end(admission, observation)
    return admission.start_record.byte_offset + observation["work"]["source_bytes_read"]


def provenance(config):
    return "codex" if source_host(config) == CODEX else "claude-code"

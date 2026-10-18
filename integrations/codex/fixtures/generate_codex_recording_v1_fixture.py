"""Reproduce the public prior-recorder fixture using synthetic environmental inputs.

The lifecycle implementation is the unmodified, hash-pinned f516d62 source. Its
observer callbacks receive a deterministic synthetic open-turn witness; this is
not a captured Codex runtime transcript. Paths, device/inode, boot ID, clock,
service bytes and prompt are synthetic BEFORE config/replay proofs are computed.
Only the disposable ledger's storage location varies, and it is not serialized.
No providers, owners, installed hosts, credentials or live configuration are used.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import types
from unittest.mock import patch

HELPERS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HELPERS))
import fixture_source_turn as source
from turn_observer import TurnAdmission, RecordAnchor

REVISION = "f516d62dda79c3ad86dc79d6b3ef6ef48c56728a"
SOURCE_SHA256 = "01ccb291e3d34150bb356823184e636548286add317382175e0efc7c4a9c93e7"


def generate():
    raw = subprocess.check_output(["git", "show", REVISION + ":integrations/codex/recording_jobs.py"], cwd=HELPERS)
    if hashlib.sha256(raw).hexdigest() != SOURCE_SHA256:
        raise ValueError("prior recorder source identity changed")
    prior = types.ModuleType("synthetic_prior_recording_jobs")
    exec(compile(raw, REVISION + ":recording_jobs.py", "exec"), prior.__dict__)
    session, turn, prompt = "recording-session", "turn-1", "Inspect the synthetic setting."
    header = source.encoded([source.header(session)])
    start = source.encoded([source.start(turn)])
    prompt_bytes = source.encoded([source.user(prompt, turn=turn)])
    admission = TurnAdmission("/synthetic/codex/sessions/recording-session.jsonl", "/synthetic/codex/sessions",
        session, turn, hashlib.sha256(prompt.encode()).hexdigest(), 1, 2,
        RecordAnchor(0, len(header), hashlib.sha256(header).hexdigest()),
        RecordAnchor(len(header), len(start), hashlib.sha256(start).hexdigest()))
    service = Path("/synthetic/mneme/service.json")
    real_read = prior.read_regular
    def read_regular(path, cap):
        return b'{"synthetic":"not opened by a native client"}' if Path(path) == service else real_read(path, cap)
    with tempfile.TemporaryDirectory() as directory, \
         patch.dict(prior.os.environ, {"CODEX_HOME": "/synthetic/codex"}), \
         patch.object(prior, "read_regular", side_effect=read_regular), \
         patch.object(prior, "admit_source_turn", return_value={"status": "admitted", "admission": admission}), \
         patch.object(prior, "observe_source_turn", return_value={"reason": "source_turn_not_closed",
             "work": {"source_bytes_read": len(start) + len(prompt_bytes)}}), \
         patch.object(prior, "_boot_id", return_value="synthetic-boot-witness"), \
         patch.object(prior.time, "time", return_value=1000.0), \
         patch.object(prior.time, "monotonic", return_value=100.0):
        config = {"state_dir": Path(directory) / "state", "service_config": service,
            "project_root": Path("/synthetic/mneme/workspace"), "memory_scope": "project",
            "recording_mode": "automatic", "recall_mode": "async", "reader_model": "gpt-6.1-sol",
            "librarian_effort": "medium"}
        event = {"session_id": session, "turn_id": turn, "prompt": prompt, "transcript_path": admission.path}
        prior.session_start(config, {**event, "source": "startup"})
        if prior.notice(config, event) != {"outcome": "admitted"} or not prior.close_turn(config, session, turn):
            raise ValueError("prior lifecycle did not produce the expected fixture")
        return prior._load(prior._path(config, session), session)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=Path(__file__).with_name("codex-recording-v1-f516d62.json"))
    args = parser.parse_args()
    args.output.write_text(json.dumps(generate(), sort_keys=True, indent=2) + "\n")

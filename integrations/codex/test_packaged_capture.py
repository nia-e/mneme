"""Finite copied-helper smoke, not installed Claude/auth/live-owner validation."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest

import install


FAKE_NATIVE = '''
import json, pathlib, sys
root = pathlib.Path(__file__).parent
snapshot = json.loads((root / "selection.json").read_text())
assert sys.argv[1:3] == ["--json", "config"]
if sys.argv[3] == "select":
    print(json.dumps(snapshot))
else:
    assert sys.argv[3] == "call"
    request = json.load(sys.stdin)
    denied = request["tool"] != "status"
    result = {"schema":"mneme.config-call.v1", "selection_digest":snapshot["digest"],
              "store":"fixture", "ok":not denied, "work":{"decoded_bytes":32}}
    if denied:
        result["error"] = {"kind":"tool", "message":"fixture capability denied"}
    else:
        result["result"] = json.loads((root / "owner.json").read_text())
    print(json.dumps(result))
'''
FAKE_CLAUDE = '''
import json, pathlib, sys
root = pathlib.Path(__file__).parent
args = sys.argv[1:]
if args == ["--version"]:
    print("2.1.300 (Claude Code)"); sys.exit(0)
if args == ["--help"]:
    print((root / "capabilities.txt").read_text()); sys.exit(0)
assert "--safe-mode" in args and "--max-turns=1" in args
assert not any(value.startswith("--json-schema") for value in args)
prompt = sys.stdin.read()
assert "OUTPUT_SCHEMA:" in prompt
option = lambda key: next(value.split("=",1)[1] for value in args if value.startswith(key + "="))
model, session = option("--model"), option("--session-id")
print(json.dumps({"type":"system", "subtype":"init", "session_id":session,
                  "model":model, "tools":[], "mcp_servers":[]}))
print(json.dumps({"type":"result", "subtype":"success", "session_id":session,
    "is_error":False, "num_turns":1, "stop_reason":"end_turn", "permission_denials":[],
    "result":'{"proposal":null}', "modelUsage":{model:{"inputTokens":4,
    "outputTokens":8, "cacheReadInputTokens":2, "cacheCreationInputTokens":1}}}))
'''
SMOKE = r'''
import hashlib, importlib, json, pathlib, shutil, sys
root = pathlib.Path(sys.argv[1]).resolve()
lib = root / "prefix" / "lib"
manifest = json.loads((root / "manifest.json").read_text())
assert shutil.which("codex") is None
assert pathlib.Path.home() == root / "home"
for name, digest in manifest.items():
    module = importlib.import_module(name[:-3])
    assert pathlib.Path(module.__file__).resolve() == lib / name, (name, module.__file__)
    assert hashlib.sha256((lib / name).read_bytes()).hexdigest() == digest
from claude_config import prepare, load_config
from claude_observer import admit_source_turn, observe_source_turn
from claude_runtime import REQUIRED_FLAGS
from assessment import runtime
from store_registry import NativeClient
from registry_runtime import selection
from rollout_primitives import digest
(root / "capabilities.txt").write_text("--print " + " ".join(REQUIRED_FLAGS))
config_path = root / "adapter.json"
prepared = prepare(memory_config=root / "memory.toml", native_cli=root / "native",
    state_dir=root / "state-claude", reader_claude=root / "claude",
    reader_model="claude-sonnet-4-5-20250929", librarian_effort="medium",
    transcripts_root=root / "transcripts", hooks_program=lib / "claude_hooks.py",
    hook_config_path=config_path, python=pathlib.Path(sys.executable).resolve())
assert prepared["installation"] == "not performed"
assert not config_path.exists() and not (root / "state-claude").exists()
config_path.write_text(json.dumps(prepared["hook_config"]))
config = load_config(config_path, cwd=str(root / "workspace"))
assert config["recording_mode"] == "automatic" and config["recall_mode"] == "off"
assert config["assessor"] == "claude_code" and "reader_codex" not in config
assert not config["_background_maintenance"]
selected = selection(config)
owner_before = (root / "owner.json").read_bytes()
with NativeClient(selected.named("fixture"), operation="explicit") as client:
    assert client.call_tool("status", {}) == {"state":"fixture-ready"}
    try:
        client.call_tool("save", {"summary":"must not write"})
    except Exception as error:
        assert str(error) == "fixture capability denied", type(error)
    else:
        raise AssertionError("capability denial missing")
assert (root / "owner.json").read_bytes() == owner_before
source = root / "transcripts" / "session.jsonl"
source.write_bytes(b"")
prompt = "Preserve exact provenance."
admission = admit_source_turn(source, sessions_root=source.parent, session_id="session",
    turn_id="prompt", expected_prompt_sha256=digest(prompt.encode()))["admission"]
rows = [{"type":"user", "uuid":"u", "parentUuid":None, "sessionId":"session",
         "message":{"role":"user", "content":prompt}},
        {"type":"assistant", "uuid":"a", "parentUuid":"u", "sessionId":"session",
         "message":{"role":"assistant", "content":[{"type":"text", "text":"Done."}]}},
        {"type":"user", "uuid":"next", "parentUuid":"a", "sessionId":"session",
         "message":{"role":"user", "content":"Next turn."}}]
source.write_text("".join(json.dumps(row)+"\n" for row in rows[:2]))
stop_receipt = {"hook_event_name":"Stop", "session_id":"session", "prompt_id":"prompt",
    "transcript_path":str(source), "last_assistant_message":"Done.", "stop_hook_active":False}
assert observe_source_turn(admission, stop_receipt=stop_receipt)["reason"] == "source_boundary_pending"
assert observe_source_turn(admission, stop_receipt=stop_receipt, boundary_receipt={
    "schema":"mneme.claude-boundary-receipt.v1", "session_id":"session", "turn_id":"prompt",
    "kind":"session_end", "next_turn_id":None, "prompt_sha256":None,
    "transcript_path":str(source)})["reason"] == "source_snapshot_ready"
source.write_text("".join(json.dumps(row)+"\n" for row in rows))
observation = observe_source_turn(admission, stop_receipt=stop_receipt, boundary_receipt={
    "schema":"mneme.claude-boundary-receipt.v1", "session_id":"session", "turn_id":"prompt",
    "kind":"next_prompt", "next_turn_id":"next-prompt", "prompt_sha256":digest(b"Next turn."),
    "transcript_path":str(source)})
assert observation["status"] == "observed", observation
assert observation["coverage"]["turn_finality"] == "unproven", observation
assert observation["coverage"]["remaining_transcript"] == "unknown", observation
# The copied helper set must include the pure shared review contract, not rely
# on this source checkout or a separately installed module on PATH.
from recording_contract import prepare as prepare_recording, validate_answer, ReviewNomination
prompt_text, context = prepare_recording(observation, selected_stores=selected.recordable)
assert prompt_text is not None, context
review = validate_answer({"proposal":{"kind":"review_nomination", "summary":"Review this source scene",
    "body":"Tentative foreground judgment only.", "evidence_ids":[context.bindings[0].evidence_id],
    "destination":"fixture", "related_memory_ids":[],
    "effect":{"kind":"core_creation", "summary":"Tentative core wording", "body":""}}}, context)
assert review["proposal"] is None and review["maintenance"] == []
assert isinstance(review["nomination"], ReviewNomination)
assert review["nomination"].effect.to_handoff()["kind"] == "core_creation"
with runtime(config, root / "scratch") as assessor:
    answer = assessor.assess(observation, timeout=5)
    assert answer["reason"] == "abstained", answer
    assert answer["provider_attempt"] and answer["usage"]["input_tokens"] == 7, answer
    assert assessor.process is None and assessor._own_cwd is None
(root / "capabilities.txt").write_text("--print --model")
with runtime(config, root / "scratch-refusal") as assessor:
    answer = assessor.assess(observation, timeout=5)
    assert answer["reason"] == "unsupported_runtime" and not answer["provider_attempt"], answer
    assert assessor.process is None and assessor._own_cwd is None
(root / "claude").write_text((root / "claude").read_text()+"\n# changed\n")
try:
    load_config(config_path, cwd=str(root / "workspace"))
except ValueError as error:
    assert "changed" in str(error)
else:
    raise AssertionError("changed executable accepted")
for name, expected in manifest.items():
    assert hashlib.sha256((lib / name).read_bytes()).hexdigest() == expected
assert not (root / "scratch").exists() and not (root / "state-claude").exists()
print(json.dumps({"status":"copied-helper-smoke-ok", "modules":len(manifest),
                  "claude":"fake", "owner":"fake", "codex_on_path":False}))
'''


@unittest.skipIf(os.name == "nt", "fake executable launchers are POSIX; native Windows has separate host tests")
class CopiedCaptureTests(unittest.TestCase):
    def test_copied_inventory_without_codex_and_finite_fake_adapter(self):
        with tempfile.TemporaryDirectory(prefix="mneme-copied-capture-") as temporary:
            root = Path(temporary).resolve()
            root.chmod(0o700)
            for name in ("prefix/lib", "home", "empty-bin", "workspace", "transcripts"):
                (root / name).mkdir(parents=True, mode=0o700)
            manifest = {}
            for name in install.PROGRAMS:
                target = root / "prefix/lib" / name
                shutil.copyfile(Path(install.__file__).with_name(name), target)
                target.chmod(0o600)
                manifest[name] = hashlib.sha256(target.read_bytes()).hexdigest()
            (root / "manifest.json").write_text(json.dumps(manifest))
            for name, script in (("native", FAKE_NATIVE), ("claude", FAKE_CLAUDE)):
                executable = root / name
                executable.write_text("#!" + sys.executable + "\n" + textwrap.dedent(script))
                executable.chmod(0o700)
            owner = {"id":"01M3CPEVDTM9PP4HJTMMBZ8HR9", "owner":{
                "url":"http://127.0.0.1:1", "db":"project", "path":None}}
            eligible = dict(read=True, write=True, core=True, context=True, recall=True,
                            record=True, maintenance=False)
            snapshot = {"schema":"mneme.config-selection.v1", "digest":"a"*64,
                "cwd":str(root / "workspace"), "config":{"background":{"enabled":True}},
                "active":[{"name":"fixture", "store":owner, "eligible":eligible}]}
            (root / "selection.json").write_text(json.dumps(snapshot))
            (root / "owner.json").write_text('{"state":"fixture-ready"}')
            # Deliberately not a real selector: only the fake native process reads it.
            (root / "memory.toml").write_text('# disposable fake selector\n')
            env = {"HOME":str(root / "home"), "PATH":str(root / "empty-bin"),
                   "PYTHONPATH":str(root / "prefix/lib"), "PYTHONDONTWRITEBYTECODE":"1",
                   "PYTHONUTF8":"1", "TMPDIR":str(root)}
            result = subprocess.run([sys.executable, "-c", SMOKE, str(root)], cwd=root / "workspace",
                env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=25, text=True)
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
            output = json.loads(result.stdout)
            self.assertEqual(output, {"status":"copied-helper-smoke-ok", "modules":len(install.PROGRAMS),
                "claude":"fake", "owner":"fake", "codex_on_path":False})
            self.assertEqual(list((root / "home").iterdir()), [])
            self.assertEqual(list(root.glob("mneme-claude-cwd-*")), [])
        self.assertFalse(root.exists())


if __name__ == "__main__":
    unittest.main()

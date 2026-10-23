"""Finite fake executable tests, NOT evidence that installed Claude honors flags.

Wire shapes follow Anthropic's CLI parser and fixtures (result/modelUsage,
assistant.message.model/content, session_id); deliberately no SDK dependency:
https://github.com/anthropics/claude-agent-sdk-python/blob/main/tests/test_message_parser.py
https://code.claude.com/docs/en/agent-sdk/cost-tracking
"""
import copy
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

import assessment
from claude_runtime import ClaudeRuntime, result_usage, subscription_environment
import host_files
import host_process
from librarian_policy import resolve

MODEL = "claude-sonnet-4-5-20250929"
FAKE = r'''#!PYTHON
import json, os, sys, time, subprocess
args=sys.argv[1:]
mode=os.environ.get("FAKE_CLAUDE_MODE","normal")
trace=os.environ["FAKE_CLAUDE_TRACE"]
def record(row):
    with open(trace,"a") as stream: stream.write(json.dumps(row)+"\n")
record({"args": args, "cwd": os.getcwd(), "pid": os.getpid()})
if args==["--version"]:
    if mode=="probe_timeout": time.sleep(5)
    print(os.environ.get("FAKE_CLAUDE_VERSION","2.1.300 (Claude Code)")); sys.exit(0)
if args==["--help"]:
    help_text="--print FLAGS" if mode!="unsupported" else "--print --model"
    if mode=="missing_isolation": help_text=help_text.replace("--safe-mode", "--safe-model")
    print(help_text)
    if mode=="probe_flood": print("x"*70000)
    sys.exit(0)
def option(key): return next(a.split("=",1)[1] for a in args if a.startswith(key+"="))
if mode=="unsupported_flags": sys.exit(2)
if mode=="no_read": time.sleep(5)
prompt=sys.stdin.read()
record({"prompt":prompt, "safety_env": {k:os.environ.get(k) for k in
    ["ANTHROPIC_API_KEY", "CLAUDECODE", "CLAUDE_CODE_MAX_RETRIES", "CLAUDE_CODE_RETRY_WATCHDOG",
     "CLAUDE_CODE_MAX_OUTPUT_TOKENS", "CLAUDE_ENABLE_STREAM_WATCHDOG", "HOME"]}})
session=option("--session-id"); model=option("--model")
def send(row):
    print(json.dumps(row,separators=(",",":")),flush=True)
if mode=="timeout": time.sleep(5)
if mode=="descendant":
    child=subprocess.Popen([sys.executable,"-c","import time;time.sleep(60)"],stdout=subprocess.DEVNULL)
    record({"child":child.pid})
init={"type":"system","subtype":"init","session_id":session,"model":model,
      "tools":["Read"] if mode=="tools_advertised" else [],"mcp_servers":[],
      "plugins":[],"skills":[],"agents":[]}
for field in ("plugins","skills","plugin_errors","agents"):
    if mode=="loaded_"+field:init[field]=["unexpected-customization"]
send(init)
if mode=="frames":
    for _ in range(1030): send({"type":"rate_limit_event","session_id":session})
if mode=="frame_cap": print("x"*524288,flush=True);sys.exit(0)
if mode=="aggregate":
    for _ in range(3): send({"type":"rate_limit_event","session_id":session,"padding":"x"*400000})
if mode=="hook": send({"type":"system","subtype":"hook_started","session_id":session})
content=[{"type":"thinking","thinking":"PRIVATE-REASONING-DO-NOT-RECEIPT","signature":"sig"},
         {"type":"text","text":"the final result is authoritative"}]
if mode=="tool": content.append({"type":"tool_use","id":"tool1","name":"Read","input":{"path":"/private"}})
send({"type":"assistant","session_id":session,"message":{"model":"claude-opus-4-6" if mode=="assistant_mismatch" else model,
    "content":content,"usage":{"input_tokens":3,"output_tokens":1}}})
answer='{"proposal":null}'
if mode=="bad_json": answer='{"proposal":null,"proposal":null}'
if mode=="invalid_proposal": answer='{"proposal":{"kind":"episode"}}'
if mode=="answer_cap": answer+=' '*6144
usage={model:{"inputTokens":3,"outputTokens":24,"cacheReadInputTokens":20,"cacheCreationInputTokens":5,"costUSD":0.01}}
if mode=="model_mismatch": usage={"claude-opus-4-6":next(iter(usage.values()))}
if mode=="multi_model": usage["claude-opus-4-6"]=dict(usage[model])
if mode=="missing_usage": usage=None
if mode=="missing_cache_write": usage[model].pop("cacheCreationInputTokens")
result={"type":"result","subtype":"error_during_execution" if mode=="crash" else "success",
        "session_id":"foreign" if mode=="session_mismatch" else session,
        "duration_ms":100,"duration_api_ms":80,"num_turns":2 if mode=="turns" else 1,
        "is_error":False,"stop_reason":"max_tokens" if mode=="truncated" else "end_turn",
        "result":answer,"usage":{"input_tokens":3,"output_tokens":1},"modelUsage":usage,
        "permission_denials":[{"tool_name":"Read"}] if mode=="denial" else []}
if mode=="error": result.update(subtype="error_max_turns",is_error=True,errors=["PRIVATE-ERROR-TEXT"])
if mode.startswith("terminal_"): result["terminal_reason"]=mode[len("terminal_"):]
if mode=="origin_human":result["origin"]={"kind":"human"}
if mode.startswith("origin_bad_"):
    result["origin"]={"kind":mode[len("origin_bad_"):]} if mode!="origin_bad_malformed" else "human"
if mode=="origin_bad_missing_kind":result["origin"]={}
if mode=="duplicate_frame":
    print('{"type":"result","type":"result"}',flush=True);sys.exit(0)
send(result)
if mode=="duplicate_result": send(result)
if mode=="hang_after_result":
    sys.stdout.close();time.sleep(5)
if mode=="exit_error": sys.exit(9)
'''


def observation():
    def ref(n):
        return {"ordinal":n,"line":n+1,"ordinal_scope":"source_turn","byte_offset":n*128,
                "line_bytes":128,"raw_line_sha256":hashlib.sha256(str(n).encode()).hexdigest()}
    return {"schema":"mneme.codex-turn-observation.v3","status":"complete","reason":"source_turn_complete",
            "evidence":[{"kind":"user_statement","ref":ref(1),"content":[{"content_index":0,"text":"no, R5"}]}],
            "boundary":ref(2),"coverage":{"source_turn":"closed_verified","memory_delivery":"not_recorded",
                "prompt":"verified","earlier_session_context":"omitted","other_host_inputs":"omitted",
                "private_reasoning":"excluded","sensitivity_review":"not_performed","missing_results":0,
                "omissions":{},"public_evidence":{"mode":"all","observed_records":1,"selected_records":1,"omitted_records":0}}}


class ClaudeRuntimeTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name);host_files.set_private(self.root,directory=True)
        self.executable=self.root/("claude.exe" if os.name=="nt" else "claude")
        self.executable.write_text(FAKE.replace("#!PYTHON","#!"+sys.executable,1))
        self.executable.chmod(0o700)
        self.trace=self.root/"trace"
        self.config={"assessor":"claude_code","reader_claude":str(self.executable),
            "reader_claude_sha256":hashlib.sha256(self.executable.read_bytes()).hexdigest(),
            "reader_model":MODEL,"librarian_effort":"medium"}
        self.env=patch.dict(os.environ,{"FAKE_CLAUDE_TRACE":str(self.trace),"FAKE_CLAUDE_MODE":"normal"})
        self.env.start();self.addCleanup(self.env.stop)
        if os.name=="nt":
            native_spawn=host_process.spawn
            def fixture_spawn(argv,**kw):
                if Path(argv[0])==self.executable: argv=[sys.executable,*argv]
                return native_spawn(argv,**kw)
            fixture=patch.object(host_process,"spawn",side_effect=fixture_spawn)
            fixture.start();self.addCleanup(fixture.stop)

    def rows(self):
        return [json.loads(line) for line in self.trace.read_text().splitlines()] if self.trace.exists() else []

    def run_mode(self,mode="normal",**kwargs):
        with patch.dict(os.environ,{"FAKE_CLAUDE_MODE":mode}),ClaudeRuntime(self.config,self.root/"scratch") as runtime:
            result=runtime.assess(observation(),**kwargs)
            self.assertIsNone(runtime.process);self.assertIsNone(runtime._own_cwd)
        return result

    def test_native_only_happy_path_and_known_usage(self):
        with patch.dict(os.environ,{"ANTHROPIC_API_KEY":"PRIVATE-INHERITED-KEY","CLAUDECODE":"nested"}):
            result=self.run_mode()
        self.assertEqual(result["reason"],"abstained");self.assertTrue(result["provider_attempt"])
        self.assertEqual(result["usage"],{"input_tokens":28,"output_tokens":24,"cached_input_tokens":20,
            "cache_write_input_tokens":5,"uncached_input_tokens":8,"reasoning_output_tokens":None,"total_tokens":52})
        rows=self.rows();self.assertEqual(len(rows),2)
        args=rows[0]["args"]
        self.assertIn("--safe-mode",args);self.assertIn("--tools=",args);self.assertIn("--setting-sources=",args)
        self.assertIn("--disallowedTools=mcp__*",args);self.assertIn("--permission-prompts=none",args)
        self.assertNotIn("--bare",args);self.assertFalse(any(a.startswith("--json-schema") for a in args))
        self.assertIn("--model="+MODEL,args);self.assertIn("--effort=medium",args)
        self.assertIn("--max-turns=1",args);self.assertIn("--no-session-persistence",args)
        self.assertNotIn("codex",args)
        self.assertIn("OUTPUT_SCHEMA:",rows[1]["prompt"])
        env=rows[1]["safety_env"];self.assertIsNone(env["ANTHROPIC_API_KEY"]);self.assertIsNone(env["CLAUDECODE"])
        self.assertEqual(env["CLAUDE_CODE_MAX_RETRIES"],"0");self.assertEqual(env["CLAUDE_CODE_RETRY_WATCHDOG"],"0")
        self.assertEqual(env["HOME"],os.environ["HOME"])
        self.assertNotIn("PRIVATE-",json.dumps(result))
        self.assertFalse(Path(rows[0]["cwd"]).exists())

    def test_known_usage_survives_invalid_answer_and_model_mismatch(self):
        expected={"bad_json":"invalid_output","answer_cap":"invalid_output","invalid_proposal":"invalid_output",
            "model_mismatch":"model_mismatch","assistant_mismatch":"model_mismatch","multi_model":"model_mismatch",
            "session_mismatch":"provider_error","turns":"provider_error","truncated":"provider_error",
            "denial":"tool_activity","error":"provider_error","exit_error":"provider_error"}
        for mode,reason in expected.items():
            with self.subTest(mode=mode):
                result=self.run_mode(mode)
                self.assertEqual(result["reason"],reason);self.assertIsNotNone(result["usage"])
                self.assertIsNone(result["proposal"]);self.assertNotIn("PRIVATE-",json.dumps(result))

    def test_cancelled_injected_or_unknown_terminal_is_not_success_but_retains_usage(self):
        for mode in ("terminal_aborted_streaming","terminal_aborted_tools","terminal_max_turns","terminal_future_reason",
                     "origin_bad_task-notification","origin_bad_channel","origin_bad_unclassified","origin_bad_future",
                     "origin_bad_malformed","origin_bad_missing_kind"):
            with self.subTest(mode=mode):
                result=self.run_mode(mode)
                self.assertEqual(result["reason"],"provider_error");self.assertIsNone(result["proposal"])
                self.assertEqual(result["usage"]["input_tokens"],28)
        for mode in ("terminal_completed","origin_human"):
            with self.subTest(mode=mode):self.assertEqual(self.run_mode(mode)["reason"],"abstained")

    def test_loaded_customizations_refused_even_without_tools(self):
        for field in ("plugins","skills","plugin_errors","agents"):
            with self.subTest(field=field):
                result=self.run_mode("loaded_"+field)
                self.assertEqual(result["reason"],"tool_activity");self.assertIsNone(result["proposal"])

    def test_metadata_is_not_an_admission_gate(self):
        with patch.dict(os.environ,{"FAKE_CLAUDE_VERSION":"3.0.0 (Claude Code)"}):
            self.assertEqual(self.run_mode()["reason"],"abstained")
        self.assertEqual(len(self.rows()),2)
        self.assertEqual(self.rows()[0]["args"][0],"-p")

    def test_unpinned_path_command_resolves_again_after_replacement(self):
        self.config.pop("reader_claude_sha256")
        self.config["reader_claude"]="claude"
        with patch.dict(os.environ,{"PATH":str(self.root)+os.pathsep+os.environ.get("PATH","")}), \
                ClaudeRuntime(self.config,self.root/"scratch") as runtime:
            self.assertEqual(runtime.assess(observation())["reason"],"abstained")
            self.executable.write_text(self.executable.read_text()+"\n# updated executable\n")
            self.assertEqual(runtime.assess(observation())["reason"],"abstained")
        self.assertEqual(len(self.rows()),4)

    def test_missing_path_and_failed_command_are_ordinary_errors_without_retry(self):
        self.config.pop("reader_claude_sha256")
        self.config.pop("reader_claude")
        with patch.dict(os.environ,{"PATH":str(self.root / "missing")}):
            result=self.run_mode()
        self.assertEqual(result["reason"],"unavailable")
        self.assertFalse(result["provider_attempt"])
        with patch.dict(os.environ,{"PATH":str(self.root)+os.pathsep+os.environ.get("PATH","")}):
            result=self.run_mode("unsupported_flags")
        self.assertTrue(result["provider_attempt"])
        self.assertEqual(result["reason"],"usage_unknown")
        self.assertEqual(len(self.rows()),1)

    def test_unknown_usage_halts_without_retry(self):
        for mode in ("missing_usage","missing_cache_write","crash","duplicate_result","duplicate_frame"):
            with self.subTest(mode=mode),patch.dict(os.environ,{"FAKE_CLAUDE_MODE":mode}),ClaudeRuntime(self.config,self.root/"scratch") as runtime:
                result=runtime.assess(observation());self.assertIsNone(result["usage"]);self.assertTrue(result["provider_attempt"])
                before=len(self.rows());again=runtime.assess(observation())
                self.assertEqual(again["reason"],"usage_unknown");self.assertFalse(again["provider_attempt"])
                self.assertEqual(len(self.rows()),before)

    def test_tools_hooks_and_bounds_fail_closed(self):
        for mode,reason in (("tool","tool_activity"),("tools_advertised","tool_activity"),("hook","tool_activity"),
                            ("frames","frame_cap"),("frame_cap","frame_cap"),("aggregate","frame_cap")):
            with self.subTest(mode=mode):
                result=self.run_mode(mode);self.assertEqual(result["reason"],reason)
                self.assertIsNone(result["proposal"]);self.assertTrue(result["provider_attempt"])

    def test_missing_unsupported_or_changed_binary_has_no_provider_attempt(self):
        self.config["reader_claude_sha256"]="0"*64
        result=self.run_mode();self.assertEqual(result["reason"],"binary_changed");self.assertFalse(result["provider_attempt"])
        self.config["reader_claude"]=str(self.root/"missing")
        self.assertEqual(self.run_mode()["reason"],"unavailable")

    def test_timeout_covers_probe_provider_and_exit(self):
        for mode in ("timeout","hang_after_result"):
            with self.subTest(mode=mode):
                with patch.dict(os.environ,{"FAKE_CLAUDE_MODE":mode}),ClaudeRuntime(self.config,self.root/"scratch") as runtime:
                    start=time.monotonic();result=runtime.assess(observation(),timeout=0.2)
                    self.assertLess(time.monotonic()-start,2)
                self.assertEqual(result["reason"],"timeout")
                self.assertTrue(result["provider_attempt"])
                if mode=="hang_after_result": self.assertIsNotNone(result["usage"])

    def test_full_schema_request_cap_precedes_provider(self):
        with ClaudeRuntime(self.config,self.root/"scratch") as runtime:
            result=runtime._fresh_assessment(lambda:("x"*assessment.MAX_PREPARED_BYTES,{}),
                instructions=lambda _:"rules",schema=lambda _:{"type":"object"},validator=lambda value,_:value,
                field="result",service_name="fixture",success=lambda _:"ok",timeout=1,frame_bytes=10,answer_bytes=10)
        self.assertEqual(result["reason"],"authored_input_limit");self.assertFalse(result["provider_attempt"])
        self.assertEqual(self.rows(),[])

    def test_blocked_stdin_write_uses_the_same_absolute_deadline(self):
        with patch.dict(os.environ,{"FAKE_CLAUDE_MODE":"no_read"}),ClaudeRuntime(self.config,self.root/"scratch") as runtime:
            start=time.monotonic()
            result=runtime._fresh_assessment(lambda:("x"*100000,{}),
                instructions=lambda _:"rules",schema=lambda _:{"type":"object"},validator=lambda value,_:value,
                field="result",service_name="fixture",success=lambda _:"ok",timeout=0.2,
                frame_bytes=100,answer_bytes=100)
            self.assertLess(time.monotonic()-start,2)
            self.assertEqual(result["reason"],"timeout");self.assertTrue(result["provider_attempt"])
            self.assertIsNone(result["usage"]);self.assertTrue(runtime._halted)
            self.assertIsNone(runtime.process);self.assertIsNone(runtime._own_cwd)

    def test_each_call_has_fresh_session_and_no_history(self):
        with ClaudeRuntime(self.config,self.root/"scratch") as runtime:
            self.assertEqual(runtime.assess(observation())["reason"],"abstained")
            self.assertEqual(runtime.assess(observation())["reason"],"abstained")
        calls=[row for row in self.rows() if row.get("args",[])[:1]==["-p"]]
        self.assertEqual(len(calls),2)
        sessions=[next(a for a in row["args"] if a.startswith("--session-id=")) for row in calls]
        self.assertNotEqual(*sessions);self.assertNotEqual(calls[0]["cwd"],calls[1]["cwd"])
        self.assertEqual(sum(row.get("args")==["--help"] for row in self.rows()),0)

    @unittest.skipIf(os.name=="nt","Unix process-group evidence; native job ownership separately tested")
    def test_descendant_cleanup_after_exited_parent(self):
        result=self.run_mode("descendant");self.assertEqual(result["reason"],"abstained")
        child=next(row["child"] for row in self.rows() if "child" in row)
        end=time.monotonic()+1
        while time.monotonic()<end:
            try: os.kill(child,0)
            except ProcessLookupError: break
            # A Linux zombie has exited but can remain until the system reaps it.
            stat=Path(f"/proc/{child}/stat")
            if stat.exists() and stat.read_text().split()[2]=="Z": break
            time.sleep(0.01)
        else: self.fail("owned descendant survived parent completion")


class ClaudeUsageTests(unittest.TestCase):
    def test_ambiguous_counts_are_unknown_not_zero(self):
        base={"subtype":"success","modelUsage":{MODEL:{"inputTokens":3,"outputTokens":4,
                 "cacheReadInputTokens":5,"cacheCreationInputTokens":6}}}
        for value in (None,True,-1,1.5,"4",1_000_001):
            row=copy.deepcopy(base);row["modelUsage"][MODEL]["outputTokens"]=value
            with self.subTest(value=value),self.assertRaises(assessment.TransportError):result_usage(row)

if __name__=="__main__": unittest.main()

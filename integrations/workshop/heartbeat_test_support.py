"""Shared isolated heartbeat fixtures; no discoverable tests or live services."""

import datetime
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


WRAPPER = Path(__file__).with_name("heartbeat.py")
FAKE = r'''
import json
import os
from pathlib import Path
import subprocess
import sys
import time

args = sys.argv[1:]
out = Path(args[args.index("--output-last-message") + 1])
stage = out.parent.name
prompt = sys.stdin.read()
mode = os.environ.get("FAKE_" + stage.upper() + "_MODE", os.environ.get("FAKE_MODE", "valid"))
if os.environ.get("FAKE_ARGV"):
    with Path(os.environ["FAKE_ARGV"]).open("a") as stream:
        stream.write(json.dumps({"stage": stage, "args": args, "prompt": prompt}) + "\n")
if mode in ("wait", "child"):
    if mode == "child":
        subprocess.Popen([sys.executable, "-c",
                          "import os,time; from pathlib import Path; "
                          "time.sleep(1.5); Path(os.environ['FAKE_SENTINEL']).write_text('escaped')"])
    Path(os.environ["FAKE_READY"]).write_text(str(os.getpid()))
    time.sleep(30)
if mode == "stdout_flood":
    os.write(1, b"x" * 65536)
    time.sleep(30)
if mode == "stderr_flood":
    os.write(2, b"x" * 65536)
    time.sleep(30)
if mode == "both_logs":
    os.write(1, b"x" * 900)
    os.write(2, b"x" * 900)
if mode == "project_work" and stage == "work":
    root = Path(args[args.index("--cd") + 1])
    project = root / "projects" / "demo"
    project.mkdir(parents=True, exist_ok=True)
    (project / "report.md").write_text("A bounded answer, with durable work.\n")
    (root / "AGENDA.md").write_text("# Workshop\nFollow up on the demo report.\n")
if mode == "failed":
    sys.exit(7)
if mode == "no_result":
    sys.exit(0)
if mode == "malformed":
    out.write_text("not JSON")
elif mode == "oversized":
    out.write_text(" " * 70000 + "{}")
else:
    out.write_text(os.environ.get("FAKE_" + stage.upper() + "_RESULT", os.environ["FAKE_RESULT"]))
print(json.dumps({"type": "fake.completed"}))
'''


class HeartbeatHarness:
    """Disposable workshop and fake Codex helpers for unittest test cases.

    Deliberately not a TestCase: importing this harness cannot collect another
    suite, and users own their normal setUp/cleanup lifecycle.
    """

    def setUp(self):
        super().setUp()
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        self.root = self.make_root("workshop")
        self.fake = self.base / "fake-codex"
        self.fake.write_text("#!" + sys.executable + "\n" + FAKE)
        self.fake.chmod(0o700)

    def make_root(self, name):
        root = self.base / name
        root.mkdir()
        (root / "artifacts").mkdir()
        (root / "artifacts" / "note.md").write_text("An actual artifact.\n")
        (root / "AGENDA.md").write_text("# Workshop\nExplore one bounded question.\n")
        return root

    def result(self, **changes):
        result = {"status": "progress", "summary": "Made a small observation.",
                  "artifacts": ["artifacts/note.md"], "next_step": "Recheck tomorrow.",
                  "memory_candidates": ["An observation, not an automatic memory write."],
                  "next_wake_seconds": 3600, "replies": []}
        result.update(changes)
        return result

    def orientation(self, **changes):
        result = {"status": "work", "summary": "An unfinished question remains.",
                  "task": "Explore one bounded question.", "effort": "normal",
                  "next_wake_seconds": 3600, "replies": []}
        result.update(changes)
        return result

    def environment(self, mode="valid", result=None, orientation=None, **extra):
        env = os.environ.copy()
        env.update(FAKE_MODE=mode, FAKE_RESULT=json.dumps(self.result() if result is None else result,
                                                          ensure_ascii=False),
                   FAKE_ORIENTATION_RESULT=json.dumps(
                       self.orientation() if orientation is None else orientation,
                       ensure_ascii=False),
                   FAKE_READY=str(self.base / "ready"),
                   FAKE_SENTINEL=str(self.base / "sentinel"),
                   FAKE_ARGV=str(self.base / "invocation.json"))
        env.update(extra)
        return env

    def command(self, action="run", root=None, options=()):
        return [sys.executable, str(WRAPPER), "--root", str(root or self.root),
                "--codex", str(self.fake), *options, action]

    def call(self, action="run", root=None, options=(), mode="valid", result=None,
             orientation=None, **environment):
        process = subprocess.run(self.command(action, root, options),
                                 env=self.environment(mode, result, orientation, **environment), capture_output=True,
                                 text=True, timeout=8)
        lines = process.stdout.splitlines()
        self.assertEqual(len(lines), 1, (process.returncode, process.stdout, process.stderr))
        payload = json.loads(lines[0])
        self.assertIsInstance(payload, dict)
        self.assertIn("status", payload)
        return process, payload

    def receipts(self, root=None):
        return sorted((root or self.root).glob("runs/*/receipt.json"))

    def invocations(self):
        path = self.base / "invocation.json"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def due(self, root=None, lane="background"):
        root = root or self.root
        state = root / "state.json"
        value = json.loads(state.read_text())
        target = value if lane == "background" else value["interactive"]
        target["next_due_at"] = (datetime.datetime.now(datetime.timezone.utc)
                                 - datetime.timedelta(seconds=1)).isoformat()
        state.write_text(json.dumps(value, ensure_ascii=False))

    def fake_receipt(self, name, schema, cycle_class, started_at, status="completed"):
        run = self.root / "runs" / name
        run.mkdir(parents=True)
        receipt = {"schema": schema, "run_id": name, "status": status,
                   "started_at": started_at.isoformat()}
        if cycle_class is not None:
            receipt["cycle_class"] = cycle_class
        (run / "receipt.json").write_text(json.dumps(receipt))
        return run / "receipt.json"

    def wake_module(self):
        specification = importlib.util.spec_from_file_location("workshop_wake_under_test",
                                                       WRAPPER.with_name("wake.py"))
        wake = importlib.util.module_from_spec(specification)
        specification.loader.exec_module(wake)
        return wake

    def heartbeat_module(self):
        sys.path.insert(0, str(WRAPPER.parent))
        self.addCleanup(lambda: sys.path.remove(str(WRAPPER.parent)))
        specification = importlib.util.spec_from_file_location("workshop_heartbeat_under_test", WRAPPER)
        heartbeat = importlib.util.module_from_spec(specification)
        specification.loader.exec_module(heartbeat)
        return heartbeat

    def queue_records(self, root=None):
        return self.wake_module().active_records(root or self.root)

    def enqueue(self, root=None, event_id="new-information", source="test", kind="notice"):
        return self.wake_module().enqueue(root or self.root, {
            "source": source, "event_id": event_id, "kind": kind,
            "summary": "A new local fact arrived.", "body": "Not an instruction.",
            "reference": "artifacts/note.md"})

    def signal(self, event_id="signal-question", root=None):
        return self.enqueue(root=root, event_id=event_id,
                            source="signal-owner", kind="signal-burst")

    def mailbox(self, event_id="mailbox-request", root=None):
        return self.enqueue(root=root, event_id=event_id,
                            source="private-mailbox", kind="message")

    def signal_options(self, *extra):
        return ("--signal-source", "signal-owner", *extra)

    def reply(self, event_id="new-information", **changes):
        value = {"source": "test", "event_id": event_id,
                 "text": "I checked the bounded question and found one useful fact."}
        value.update(changes)
        return value

    def receipt(self, root=None):
        paths = self.receipts(root)
        self.assertEqual(len(paths), 1, paths)
        return paths[0], json.loads(paths[0].read_text())

    def assert_rejected(self, result, root):
        process, payload = self.call(root=root, result=result)
        self.assertEqual(process.returncode, 1, payload)
        path, receipt = self.receipt(root)
        self.assertEqual(receipt["status"], "invalid_result", receipt)
        self.assertEqual(receipt["codex_exit_code"], 0, receipt)
        self.assertEqual(payload["run_id"], path.parent.name)

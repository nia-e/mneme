"""Synthetic recording-job sandbox, clock and assessment fixtures.

The calling test owns cleanup; close() also ends a loop iteration's patches early.
There are deliberately no discoverable tests or TestCase subclasses here.
"""
from contextlib import ExitStack
import hashlib
import os
from pathlib import Path
import tempfile
from unittest.mock import patch

import fixture_source_turn as source
import recording_contract as contract
import recording_jobs as jobs


DB_ID = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
OTHER_DB = "01ARZ3NDEKTSV4RRFFQ69G5FAW"
SESSION = "recording-session"
PROMPT = "no, R5"


class FakeRuntime:
    def __init__(self, owner, *, reason="proposed", kind="lesson", associate=False, routing=False):
        self.owner, self.reason, self.kind, self.associate = owner, reason, kind, associate
        self.routing = routing
        self.calls = []

    def assess(self, observation, overlap, *, timeout):
        self.calls.append((observation, overlap, timeout))
        self.owner.checks.assertEqual(self.owner.state()["jobs"][0]["phase"], "assessing")
        result = {"reason": self.reason, "provider_attempt": True,
                  "usage": {"input_tokens": 20, "output_tokens": 5}}
        if self.reason == "proposed":
            _, context = contract.prepare(observation, overlap)
            self.owner.checks.assertIsInstance(context, contract.ValidationContext)
            binding = context.bindings[0]
            citation = contract.Citation(binding.evidence_id, binding.kind,
                                         binding.source_ref_json, binding.source_field, binding.rendering)
            target = context.association_bindings[0] if self.associate else None
            citations = [citation]
            if target is not None and target.origin == "delivery":
                marker = next(b for b in context.bindings if b.kind == "memory_delivery")
                citations.append(contract.Citation(marker.evidence_id, marker.kind,
                                                   marker.source_ref_json, marker.source_field,
                                                   marker.rendering))
            result["proposal"] = contract.Proposal(self.kind, "User chose R5.",
                                                   "The user corrected the target to R5.", tuple(citations),
                                                   target)
            if self.routing:
                result["proposal"] = contract.validate_answer({"proposal": {
                    "kind": "lesson", "summary": "Use R5 under the chosen compatibility constraint.",
                    "body": "The user explicitly selected R5 rather than the delivered R6 advice.",
                    "evidence_ids": [b.evidence_id for b in context.bindings
                                     if b.kind in ("user_statement", "memory_delivery")],
                    "associate_with": None, "routing_judgment": {
                        "target": "shown001", "sign": "weaken", "conditions": "Compatibility requires R5.",
                        "rationale": "The user's explicit constraint rules out R6.", "corrects": None}}}, context)["proposal"]
        return result



class RecordingFixture:
    def __init__(self, checks):
        self.checks = checks
        self._cleanup = ExitStack()
        checks.addCleanup(self.close)
        temporary = tempfile.TemporaryDirectory()
        self._cleanup.callback(temporary.cleanup)
        self.root = Path(temporary.name)
        self.home = self.root / "codex-home"
        self.sessions = self.home / "sessions"
        self.sessions.mkdir(parents=True)
        self.service = self.root / "synthetic-service.json"
        self.service.write_text('{"synthetic":"not opened by any native client"}')
        self.config = {"state_dir": self.root / "state", "service_config": self.service,
                       "project_root": self.root, "memory_scope": "project",
                       "recording_mode": "automatic", "recall_mode": "async",
                       "reader_model": "gpt-6.1-sol", "librarian_effort": "medium"}
        self.now = 1000.0
        self.monotonic = 100.0
        self.boot = "synthetic-boot-witness"
        environment = patch.dict(os.environ, {"CODEX_HOME": str(self.home)})
        environment.start(); self._cleanup.callback(environment.stop)
        clock = patch("recording_jobs.time.time", side_effect=lambda: self.now)
        clock.start(); self._cleanup.callback(clock.stop)
        monotonic = patch("recording_jobs.time.monotonic", side_effect=lambda: self.monotonic)
        monotonic.start(); self._cleanup.callback(monotonic.stop)
        boot = patch("recording_jobs._boot_id", side_effect=lambda: self.boot)
        boot.start(); self._cleanup.callback(boot.stop)
        identity = patch("hook_recall.resolve_project_identity",
                         return_value={"outcome": "ok", "db_id": DB_ID, "native_work": {"decoded_bytes": 100}})
        self.identity = identity.start(); self._cleanup.callback(identity.stop)
        overlap = patch("hook_recall.collect_overlap",
                        return_value={"outcome": "empty", "cards": [], "db_id": DB_ID})
        self.overlap = overlap.start(); self._cleanup.callback(overlap.stop)
        self.reservations, self.accounts, self.writes = [], [], []

    def close(self):
        self._cleanup.close()

    def path(self, session=SESSION):
        return self.sessions / (session + ".jsonl")

    def append(self, rows, session=SESSION):
        with self.path(session).open("ab") as stream:
            stream.write(source.encoded(rows))

    def event(self, turn="turn-1", *, session=SESSION, prompt=PROMPT):
        return {"session_id": session, "turn_id": turn, "prompt": prompt,
                "transcript_path": str(self.path(session))}

    def startup(self, session=SESSION, *, source_name="startup"):
        jobs.session_start(self.config, {"session_id": session, "source": source_name})

    def source_start(self, turn="turn-1", *, session=SESSION, prompt=PROMPT):
        if not self.path(session).exists():
            self.append([source.header(session)], session)
        self.append([source.start(turn), source.context(turn),
                     source.user(prompt, identifier="prompt-" + turn, turn=turn)], session)

    def source_complete(self, turn="turn-1", *, session=SESSION):
        self.append([source.assistant("R5, corrected.", identifier="answer-" + turn, turn=turn),
                     source.event("task_complete", turn)], session)

    def begin(self, turn="turn-1", *, session=SESSION):
        self.startup(session)
        self.source_start(turn, session=session)
        result = jobs.notice(self.config, self.event(turn, session=session))
        self.checks.assertEqual(result["outcome"], "admitted", result)
        return self.state(session)["jobs"][-1]

    def delivery_packet(self, *, turn="turn-1", text="Exact Mneme hook output"):
        return source.refresh_delivery_packet({"schema": "mneme.codex-memory-delivery.v4", "session_id": SESSION,
                "turn_id": turn, "rendered_text": text,
                "rendered_sha256": hashlib.sha256(text.encode()).hexdigest(),
                "displayed": [{"db_id": DB_ID, "node_id": OTHER_DB, "kind": "semantic",
                               "shown_summary": "Earlier mechanism.",
                               "full_get_fingerprint": "a" * 64}], "concerns": []})

    def closed(self):
        job = self.begin()
        self.source_complete()
        jobs.close_turn(self.config, SESSION, "turn-1")
        return job

    def state(self, session=SESSION):
        return jobs._load(jobs._path(self.config, session), session)

    def mutate(self, mutation, session=SESSION):
        ok, _ = jobs._transaction(self.config, session, lambda data: (mutation(data), True))
        self.checks.assertTrue(ok)

    def reserve(self, key):
        self.checks.assertEqual(self.state()["jobs"][0]["phase"], "assessing")
        self.reservations.append(key)
        return True

    def account(self, key, result):
        self.accounts.append((key, result))
        return True

    def native_write(self, config, job, timeout):
        self.checks.assertEqual(self.state()["jobs"][0]["phase"], "write_intent")
        self.writes.append((config, dict(job), timeout))
        return {"db": "project", "db_id": DB_ID, "id": OTHER_DB,
                "readback_status": "verified", "replayed": False}

    def step(self, runtime=None, **callbacks):
        runtime = runtime or FakeRuntime(self)
        worked = jobs.step(self.config, SESSION, runtime,
                           callbacks.get("reserve", self.reserve),
                           callbacks.get("account", self.account),
                           native_write=callbacks.get("native_write", self.native_write),
                           native_maintenance=callbacks.get("native_maintenance"))
        return worked, runtime

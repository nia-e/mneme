"""Provider-free episode flow; scripted selection is NOT model-quality evidence.

Only native-client and app-server I/O are faked. Collection/readback, selector
projection, answer validation, worker accounting and hook delivery are real.
"""
from __future__ import annotations

import json
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import hook_recall
import hooks
import reader_worker
from reader_contract import PROMPT_PREFIX
from reader_runtime import ReaderRuntime
from librarian_policy import resolve
from test_hook_recall import FakeClient, ID1, ID2, ID3, ID4, ID5, ID6, ID7, discovery_metadata, reference_origin


class EpisodeReaderFlowTests(unittest.TestCase):
    def test_same_summary_episodes_select_by_metadata_and_survive_delivery(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            store = root / ".mneme" / "codex-memory.db"
            store.parent.mkdir()
            store.write_bytes(b"fixture only; never opened")
            service = root / "service.json"
            service.write_text(json.dumps({
                "binary": str(root / "not-used"), "project_db": str(store),
                "working_directory": str(root), "state_dir": str(root / "private"),
                "port": 18765,
            }))
            config = {"service_config": service, "project_root": root,
                      "state_dir": root / "state", "memory_mode": "async",
                      "reader_model": "gpt-6.1-sol", "librarian_effort": "medium"}
            summary = "The workshop goose reappeared in the release joke."
            episodes = [
                {"id": ID1, "kind": "episode", "episode_id": ID3,
                 "edition_id": ID1, "revision": 1, "current_edition_id": ID5,
                 "occurred": {"kind": "point", "at": 12}, "recorded_at": 15,
                 "edition_recorded_at": 20, "thread": "workshop",
                 "recording_session": "late-mac-recap", "origins": [reference_origin(ID1, anchor_id=ID6)],
                 "occurrence_contexts": [{"namespace": "session", "key": "draft"}]},
                {"id": ID2, "kind": "episode", "episode_id": ID4,
                 "edition_id": ID2, "revision": 2, "current_edition_id": ID2,
                 "occurred": {"kind": "point", "at": 12}, "recorded_at": 27,
                 "edition_recorded_at": 30, "thread": "workshop",
                 "recording_session": "pi-release", "origins": [{"kind": "lexical"},
                     {"kind": "reference", "anchor": {"kind": "episode", "identity": {
                         "episode_id": ID3, "edition_id": ID1, "revision": 1}},
                      "from": ID1, "to": ID2, "edge_kind": "Bridge", "body_anchor": {"start": 0, "end": 10}},
                     reference_origin(ID2, anchor_id=ID6)],
                 "occurrence_contexts": [{"namespace": "session", "key": "release"}]},
            ]
            nodes = {episode["id"]: {
                "id": episode["id"], "status": "active", "summary": summary,
                "summary_truncated": False, "created": episode["edition_recorded_at"],
                "provenance": {"type": "external", "source": {
                    "namespace": "codex", "key": episode["id"],
                    "reference": "codex://test/" + episode["id"],
                    "session": episode["recording_session"]}},
                "memory_kind": {"kind": "episode", "episode": {
                    key: episode[key] for key in
                    ("episode_id", "revision", "occurred", "recorded_at", "thread", "occurrence_contexts")}},
            } for episode in episodes}
            context = {"schema": "mneme.context.v6", "core": [], "primary": [],
                       "expansions": [], "episodes": episodes, **discovery_metadata(),
                       "observation": {"schema": 1, "learning": "disabled", "cards": [
                           {"node_id": episode["id"], "lane": "episodic",
                            "card_sha256": "a" * 64, "graph_path": None}
                           for episode in episodes]}}
            catalog = [{"db": "project", "name": "project", "state": "open",
                        "configured_path": str(store), "db_id": ID7}]

            # Choose each episode in an independent session. The selection rule
            # sees only the actual projected prompt, not collector objects/IDs.
            for index, wanted in enumerate(episodes):
                with self.subTest(thread=wanted["thread"]):
                    event = {"session_id": "s" + str(index), "turn_id": "t1",
                             "cwd": str(root), "prompt": "Recall this workshop event: "
                             + json.dumps({"context": wanted["occurrence_contexts"]})}
                    self.assertEqual(reader_worker.notice(config, event, True)["outcome"], "queued")
                    client = FakeClient(catalog, context, nodes)
                    prompts, requests, notifications = [], [], []

                    def start(runtime, _deadline):
                        runtime._thread_id = "scripted-thread"
                        runtime.cwd = root

                    def request(runtime, method, params, _deadline, **_kwargs):
                        self.assertEqual(method, "turn/start")
                        requests.append(params)
                        text = params["input"][0]["text"]
                        self.assertTrue(text.startswith(PROMPT_PREFIX))
                        packet = json.loads(text[len(PROMPT_PREFIX):])
                        prompts.append(packet)
                        cue = packet["dialogue"][0]["text"]
                        target = json.loads(cue[cue.index("{"):])
                        selected = [card["id"] for card in packet["cards"]
                                    if card.get("kind") == "episode"
                                    and card.get("occurrence_contexts") == target["context"]]
                        usage = {"inputTokens": 120, "cachedInputTokens": 20,
                                 "cacheWriteInputTokens": 0, "outputTokens": 15,
                                 "reasoningOutputTokens": 4, "totalTokens": 135}
                        notifications.extend([
                            {"method": "item/completed", "params": {
                                "threadId": runtime._thread_id, "turnId": "scripted-turn",
                                "item": {"type": "agentMessage", "text": json.dumps(
                                    {"selected_ids": selected, "concerns": []})}}},
                            {"method": "thread/tokenUsage/updated", "params": {
                                "threadId": runtime._thread_id, "turnId": "scripted-turn",
                                "tokenUsage": {"last": usage, "total": dict(usage)}}},
                            {"method": "turn/completed", "params": {
                                "threadId": runtime._thread_id,
                                "turn": {"id": "scripted-turn", "status": "completed"}}},
                        ])
                        return {"turn": {"id": "scripted-turn"}}

                    with (patch("hook_recall.McpClient", return_value=client),
                          patch.object(ReaderRuntime, "_start", start),
                          patch.object(ReaderRuntime, "_request", request),
                          patch.object(ReaderRuntime, "_next", lambda *_: notifications.pop(0))):
                        worker = threading.Thread(target=lambda: reader_worker.serve(
                            config, event["session_id"], runtime_factory=ReaderRuntime, idle_seconds=10))
                        worker.start()
                        try:
                            state_path, _ = reader_worker._paths(config, event["session_id"])
                            deadline = time.monotonic() + 5
                            while json.loads(state_path.read_text())["ready"] is None and time.monotonic() < deadline:
                                time.sleep(.01)
                            self.assertEqual(len(requests), 1)
                            self.assertEqual(notifications, [])
                            self.assertEqual([name for name, _ in client.calls],
                                             ["databases", "recall_context", "get", "get"])
                            self.assertEqual([card["summary"] for card in prompts[0]["cards"]], [summary, summary])
                            for projected, expected in zip(prompts[0]["cards"], episodes):
                                self.assertEqual({key: projected[key] for key in expected}, expected)
                            self.assertLessEqual(len(requests[0]["input"][0]["text"].encode()),
                                                 resolve(config).selector_prompt_bytes)
                            self.assertNotEqual(prompts[0]["cards"][0]["fingerprint"],
                                                prompts[0]["cards"][1]["fingerprint"])
                            state_path, _ = reader_worker._paths(config, event["session_id"])
                            state = json.loads(state_path.read_text())
                            self.assertEqual((state["attempts"], state["input_tokens"], state["output_tokens"]),
                                             (1, 120, 15))
                            self.assertFalse(state["unknown_usage"])
                            self.assertIsNotNone(state["ready"], state.get("last_selection"))
                            ready = state["ready"]["cards"]
                            self.assertEqual(len(ready), 1)
                            self.assertEqual({key: ready[0][key] for key in wanted}, wanted)

                            with patch("hooks._reader_worker", return_value=reader_worker):
                                delivered = hooks.handle_event({**event, "hook_event_name": "PostToolUse"}, config)
                                self.assertIn("hookSpecificOutput", delivered)
                                self.assertEqual(hooks.handle_event({**event, "hook_event_name": "PostToolUse"}, config), {})
                            text = delivered["hookSpecificOutput"]["additionalContext"]
                            shown, _ = json.JSONDecoder().raw_decode(text[text.index("["):])
                            self.assertEqual(len(shown), 1)
                            self.assertEqual({key: shown[0][key] for key in wanted}, wanted)
                            self.assertEqual(shown[0]["summary"], summary)
                            self.assertLessEqual(len(text.encode()), hooks.MAX_CONTEXT_BYTES)
                            state = json.loads(state_path.read_text())
                            self.assertEqual(state["emitted"], [[wanted["id"], ready[0]["fingerprint"]]])
                        finally:
                            reader_worker.end_session(config, event["session_id"])
                            worker.join(3)
                            self.assertFalse(worker.is_alive())


if __name__ == "__main__":
    unittest.main()

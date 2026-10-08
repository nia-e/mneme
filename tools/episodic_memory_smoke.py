#!/usr/bin/env python3
"""Finite installed-artifact episode smoke; fresh stores and loopback only.

Builds and installs nothing. Requires a matched-feature CLI/MCP pair and the
previous capture-v1 pair. Cached embeddings are allowed; downloads and paid APIs
are not. The receipt records failed as well as completed stages. It is evidence
of these exercised paths, not an exhaustive migration/compatibility proof.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
from capture_links_smoke import (  # noqa: E402
    McpClient, Stdio, artifact, claim, cli, expect_http_error, need,
)
from memory_library_smoke import Host, one_db  # noqa: E402

MAX_EPISODE_RESPONSE = 32 * 1024
ADMISSION_WORDS = (
    "catalog", "generation", "index contract", "unsupported store", "unsupported vector",
    "single-graph", "single_graph", "upgrade required",
    "capture requires a fresh capture-enabled store; create a separate database with `mnemed --db <path> capture init`",
)


def authored(key: str, **extra) -> dict:
    value = {
        "source": {"namespace": "episode-smoke", "key": key,
                   "reference": f"smoke://episodes/{key}"},
        "summary": f"Violet lantern episode {key}",
        "body": f"Disposable lived scene {key}. We watched the violet lantern.",
        "thread": "lantern",
        "occurred": {"kind": "point", "at": 1_790_000_000_000},
    }
    value.update(extra)
    return value


def bounded(value):
    # The app enforces the compact payload budget; direct-owner MCP adds two
    # identity-envelope fields outside it. Do not count Python's pretty spaces.
    payload = {key: item for key, item in value.items() if key not in ("db", "db_id")}
    need(len(json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode()) <= MAX_EPISODE_RESPONSE,
         "episode response exceeded the 32 KiB presentation budget")
    return value


def identity(value: dict) -> tuple[str, str]:
    root, edition = value.get("episode_id"), value.get("edition_id")
    need(isinstance(root, str) and len(root) == 26 and isinstance(edition, str)
         and len(edition) == 26, f"episode response lacks root/edition IDs: {value!r}")
    return root, edition


def admission_refusal(text: str) -> None:
    need(any(word in text.lower() for word in ADMISSION_WORDS),
         f"refusal was not recognizably generation admission: {text[-1000:]}")


def canonical_hash(db: Path) -> str:
    # Hash only after every owner process has exited. Lock files are deliberately
    # not treated as canonical state. A nonempty WAL would make this check false
    # evidence, so refuse it rather than hashing an incomplete logical store.
    for suffix in ("-wal", "-journal"):
        sidecar = Path(str(db) + suffix)
        need(not sidecar.exists() or sidecar.stat().st_size == 0,
             f"quiescent canonical hash requires an empty/absent {sidecar.name}")
    return artifact(db)["sha256"]


def has_reference(page: dict, source: str, target: str) -> bool:
    return any(row.get("edge", {}).get("from") == source
               and row.get("edge", {}).get("to") == target
               and row.get("edge", {}).get("kind") == "DerivedFrom"
               for row in page.get("items", []))


def all_ids(value) -> set[str]:
    """Inspect only identity-bearing response fields, not arbitrary body text."""
    if isinstance(value, list):
        return set().union(*(all_ids(item) for item in value)) if value else set()
    if isinstance(value, dict):
        ids = {item for key, item in value.items()
               if key in ("id", "node_id", "edition_id") and isinstance(item, str)}
        for item in value.values():
            if isinstance(item, (dict, list)):
                ids.update(all_ids(item))
        return ids
    return set()


def mixed_context(value, root_id, latest_id, old_id, semantic_ids, *, limit=32768):
    need(value.get("schema") == "mneme.context.v5", "ordinary recall did not emit context.v5")
    encoded = json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()
    need(len(encoded) <= limit and value["usage"]["content_bytes"] == len(encoded),
         "mixed context does not share the exact serialized byte budget")
    need("probationary" not in value, "successor context retained an obsolete probation lane")
    lanes = ("core", "primary", "expansions", "episodes")
    need(value["usage"]["items"] == sum(len(value[lane]) for lane in lanes),
         "episode cards escaped the shared context item accounting")
    episodes = value["episodes"]
    need(value["episodic_retrieval"]["state"] == "searched"
         and value["episodic_retrieval"]["mode"] == "lexical", "episodic coverage missing")
    need(any(card["episode_id"] == root_id and card["id"] == latest_id for card in episodes),
         "ordinary mixed recall omitted the latest episode edition")
    need(old_id not in all_ids(episodes), "ordinary mixed recall included a historical edition")
    for card in episodes:
        need(card["kind"] == "episode" and card["id"] == card["edition_id"] == card["current_edition_id"],
             "episodic lane lost its typed latest-edition identity")
    semantic = [card for lane in lanes[:-1] for card in value[lane]]
    need(not any(card.get("kind") == "episode" for card in semantic)
         and not ({root_id, latest_id} & all_ids(semantic)), "episode leaked into semantic lanes")
    need(bool(set(semantic_ids) & all_ids(semantic)), "mixed context has no positive semantic hit")
    return value


class Smoke:
    def __init__(self, options, root: Path, receipt: dict):
        self.options, self.root, self.receipt = options, root, receipt
        self.env = os.environ.copy()
        self.env.pop("MNEME_DB", None)
        self.env.update({"MNEME_RERANK": "0", "GIT_CEILING_DIRECTORIES": str(root),
                         "HF_HUB_OFFLINE": "1", "TRANSFORMERS_OFFLINE": "1",
                         "HTTP_PROXY": "http://127.0.0.1:9", "HTTPS_PROXY": "http://127.0.0.1:9",
                         "ALL_PROXY": "http://127.0.0.1:9", "NO_PROXY": "127.0.0.1,::1,localhost"})
        # The existing native bridge launches from this Python process, not env
        # passed to Host. Keep its environment equally local/offline.
        for name in ("http_proxy", "https_proxy", "all_proxy", "no_proxy"):
            self.env[name] = self.env[name.upper()]
        os.environ.pop("MNEME_DB", None)
        os.environ.update(self.env)
        os.environ["MNEME_CLIENT_BINARY"] = str(options.mnemed)
        self.db = root / "episodes.db"
        self.source = root / "capture-v1.db"

    @contextmanager
    def check(self, name):
        result = {"name": name, "status": "running"}
        self.receipt["checks"].append(result)
        start = time.monotonic()
        try:
            yield result
        except Exception as error:
            result.update(status="failed", error=str(error)[-4000:])
            raise
        else:
            result["status"] = "passed"
        finally:
            result["duration_ms"] = round((time.monotonic() - start) * 1000)

    def local(self, *args, payload=None, ok=True, db=None, old=False):
        return cli(self.options.old_mnemed if old else self.options.mnemed,
                   self.root, self.env, db or self.db, *args, payload=payload, ok=ok)

    def episode(self, *args, **kwargs):
        result = self.local("episode", *args, **kwargs)
        return bounded(result) if kwargs.get("ok", True) else result

    def upgrade_and_fence(self):
        with self.check("old_capture_source_detached_upgrade") as evidence:
            initialized = self.local("capture", "init", db=self.source, old=True)
            need(initialized.get("status") == "initialized", "old capture init failed")
            self.semantic = claim("predecessor", summary="Violet lantern lesson: distinguish missing presentation from missing facts.", active=True)
            made = self.local("capture", "add", "--input", "-", payload=self.semantic,
                              db=self.source, old=True)
            self.semantic_id = made["id"]
            original = self.local("get", self.semantic_id, "--body", db=self.source, old=True)
            old_host = Stdio(self.options.old_mcp, self.root, self.env, self.source, "operator")
            try:
                rows = old_host.tool("databases", {})
                need(len(rows) == 1 and rows[0]["db"] == "project", "old owner database catalog mismatch")
                self.db_id = rows[0]["db_id"]
            finally:
                old_host.stop()
            before = canonical_hash(self.source)
            upgrade = self.local("single-graph-upgrade", "--backend", "sqlite",
                                 "--output", str(self.db), db=self.source)
            need(upgrade.get("status") == "upgraded_copy" and self.db.is_file(),
                 "detached upgrade did not publish successor output")
            after = canonical_hash(self.source)
            need(before == after, "detached upgrade changed canonical source bytes")
            got = self.local("get", self.semantic_id, "--body")
            for field in ("summary", "body"):
                need(got.get(field) == original.get(field), f"upgrade changed semantic {field}")
            old_source = original["provenance"]["source"]
            new_source = got["provenance"]["source"]
            need(got["provenance"].get("type") == original["provenance"].get("type")
                 and got.get("status") == "active", "upgrade changed source kind or Active membership")
            need({key: value for key, value in new_source.items() if key != "request_codec"}
                 == old_source, "upgrade changed original capture source fields")
            need(isinstance(new_source.get("request_digest_sha256"), str)
                 and len(new_source["request_digest_sha256"]) == 64
                 and new_source.get("request_codec") == "capture_v1",
                 "upgrade did not preserve an explicit capture_v1 proof")
            new_replay = {key: value for key, value in self.semantic.items() if key != "active"}
            replay = self.local("capture", "add", "--input", "-", payload=new_replay)
            need(replay.get("id") == self.semantic_id and replay.get("replayed") is True,
                 "detached upgrade did not preserve capture proof")
            evidence.update(source_sha256_before=before, source_sha256_after=after,
                            db_id=self.db_id, node_id=self.semantic_id, upgrade=upgrade)

        with self.check("old_cli_and_mcp_refuse_successor_without_mutation") as evidence:
            refusals = {}
            for name, argv, payload in (
                ("cli_read", ("get", self.semantic_id), None),
                ("cli_write", ("capture", "add", "--input", "-"), claim("must-not-write")),
            ):
                before = canonical_hash(self.db)
                refusal = self.local(*argv, payload=payload, old=True, ok=False)
                admission_refusal(refusal)
                need(canonical_hash(self.db) == before, f"{name} mutated canonical successor")
                refusals[name] = refusal[-1500:]
            for name, tool, arguments in (
                ("mcp_read", "get", {"db": "project", "id": self.semantic_id}),
                ("mcp_write", "capture", {"db": "project", **claim("must-not-write")}),
            ):
                before = canonical_hash(self.db)
                old_host = None
                try:
                    old_host = Stdio(self.options.old_mcp, self.root, self.env, self.db, "operator")
                except Exception as error:
                    # Eager registry admission can refuse before initialize. Do
                    # not count a random launch/protocol error as a safe refusal.
                    log = (self.root / "operator-stdio.log").read_text(errors="replace")
                    refusal = f"{error}\n{log[-3000:]}"
                else:
                    try:
                        refusal = old_host.tool(tool, arguments, ok=False)
                    finally:
                        old_host.stop()
                admission_refusal(refusal)
                need(canonical_hash(self.db) == before, f"{name} mutated canonical successor")
                refusals[name] = refusal[-1500:]
            evidence["refusals"] = refusals

    def cli_roundtrip(self):
        with self.check("cli_append_revision_replay_conflict_and_immutable_history") as evidence:
            self.first_input = authored("cli-first")
            first = self.episode("append", "--input", "-", payload=self.first_input)
            self.root_id, self.first_id = identity(first)
            need(self.root_id == self.first_id and first.get("revision") == 0,
                 "initial root and edition did not coincide")
            replay = self.episode("append", "--input", "-", payload=self.first_input)
            need(identity(replay) == identity(first) and replay.get("replayed") is True,
                 "append exact retry did not replay")
            conflict = self.episode("append", "--input", "-",
                payload={**self.first_input, "summary": "Changed same source identity"}, ok=False)
            need("conflict" in conflict.lower(), "changed author request did not conflict")
            before = self.episode("get", self.root_id, "--body")
            need(before.get("body") == self.first_input["body"], "episode body readback differs")
            lesson = claim("episode-lesson", summary="Use the verified lantern color.",
                links=[{"to": self.first_id, "kind": "derived_from", "weight": 0.8}])
            self.lesson_id = self.local("capture", "add", "--input", "-", payload=lesson)["id"]
            self.revision_input = authored("cli-correction", expected_edition_id=self.first_id,
                reason="Correct the color without rewriting the original account.",
                summary="Amber lantern editorial correction", body="The lantern was amber, not violet.")
            revised = self.episode("revise", self.root_id, "--input", "-", payload=self.revision_input)
            self.latest_id = identity(revised)[1]
            need(identity(revised)[0] == self.root_id and self.latest_id != self.first_id
                 and revised.get("revision") == 1, "revision did not create a new edition")
            current = self.episode("get", self.root_id, "--body")
            original = self.episode("get", self.root_id, "--edition-id", self.first_id, "--body")
            need(identity(current)[1] == self.latest_id and current.get("body") == self.revision_input["body"],
                 "root did not resolve current edition")
            for field in ("episode_id", "edition_id", "revision", "summary", "body", "source", "tags",
                          "occurred", "recorded_at", "edition_recorded_at", "revises", "edit_reason"):
                need(original.get(field) == before.get(field), f"historical edition changed {field}")
            need(original.get("is_current") is False and original.get("current_edition_id") == self.latest_id,
                 "historical read did not distinguish old edition from current head")
            history = self.episode("history", self.root_id, "--limit", "1")
            need(len(history["items"]) == 1 and history.get("next"), "history failed to page")
            second = self.episode("history", self.root_id, "--limit", "1", "--after", history["next"])
            ids = {identity(row)[1] for page in (history, second) for row in page["items"]}
            need(ids == {self.first_id, self.latest_id} and not second.get("next"), "history cursor lost an edition")
            again = self.episode("revise", self.root_id, "--input", "-", payload=self.revision_input)
            need(identity(again)[1] == self.latest_id and again.get("replayed") is True,
                 "revision exact retry did not precede expected-head CAS")
            stale = authored("cli-stale", expected_edition_id=self.first_id, reason="Stale editor.")
            refusal = self.episode("revise", self.root_id, "--input", "-", payload=stale, ok=False)
            need("conflict" in refusal.lower(), "stale new edit did not conflict")
            evidence.update(episode_id=self.root_id, original_edition=self.first_id, current_edition=self.latest_id)

        with self.check("cli_bidirectional_references_keep_old_edition"):
            for anchor in (self.lesson_id, self.first_id):
                page = self.episode("references", anchor)
                need(has_reference(page, self.lesson_id, self.first_id),
                     f"incident references missed directed evidence at {anchor}")
            need(not has_reference(self.episode("references", self.latest_id), self.lesson_id, self.latest_id),
                 "editing silently reparented historical evidence")

        with self.check("cli_lexical_current_lane_and_typed_mixed_recall"):
            searched = self.episode("search", "amber", "--thread", "lantern", "--limit", "8")
            need(searched.get("mode") == "lexical" and self.latest_id in all_ids(searched),
                 "episode lexical search did not return the current edition")
            need(self.first_id not in all_ids(searched), "lexical search returned an old edition")
            timeline = self.episode("list", "--axis", "occurred", "--thread", "lantern", "--limit", "8")
            need(self.latest_id in all_ids(timeline), "timeline omitted current edition")
            recalled = self.local("recall-context", "violet lantern amber", "--k", "8", "--depth", "2")
            mixed_context(recalled, self.root_id, self.latest_id, self.first_id,
                          {self.semantic_id, self.lesson_id})
            small = self.local("recall-context", "violet lantern amber", "--k", "8", "--depth", "2",
                               "--max-content-bytes", "4096")
            mixed_context(small, self.root_id, self.latest_id, self.first_id,
                          {self.semantic_id, self.lesson_id}, limit=4096)

        with self.check("cli_malformed_request_does_not_create_unknown_path"):
            for index, payload in enumerate((authored("bad", tags=["core"]),
                                             authored("oversize", summary="x" * 2049),
                                             {"summary": "No source"})):
                parent = self.root / f"absent-{index}"
                self.episode("append", "--input", "-", payload=payload,
                             db=parent / "must-not-exist.db", ok=False)
                need(not parent.exists(), "malformed episode created an unknown parent")

    def mcp_roundtrip(self):
        with self.check("stdio_operator_episode_parity_and_bounded_errors"):
            host = Stdio(self.options.mcp, self.root, self.env, self.db, "operator")
            try:
                got = bounded(host.tool("episode", {"db": "project", "action": "get",
                    "episode_id": self.root_id, "body": True}))
                need(identity(got)[1] == self.latest_id and got["body"] == self.revision_input["body"],
                     "stdio get differs from CLI")
                payload = authored("stdio-first")
                made = bounded(host.tool("episode", {"db": "project", "action": "append", **payload}))
                replay = bounded(host.tool("episode", {"db": "project", "action": "append", **payload}))
                need(identity(made) == identity(replay) and replay.get("replayed") is True,
                     "stdio exact replay failed")
                refusal = host.tool("episode", {"db": "project", "action": "list", "limit": 33}, ok=False)
                need(len(refusal.encode()) < 4096 and "limit" in refusal.lower(), "stdio limit refusal is unbounded/unrelated")
                history = bounded(host.tool("episode", {"db": "project", "action": "history", "episode_id": self.root_id}))
                need({identity(row)[1] for row in history["items"]} == {self.first_id, self.latest_id},
                     "stdio history differs from CLI")
                self.stdio_context = mixed_context(host.tool("recall_context", {
                    "db": "project", "text": "violet lantern amber", "k": 8, "depth": 2}),
                    self.root_id, self.latest_id, self.first_id, {self.semantic_id, self.lesson_id})
            finally:
                host.stop()

        with self.check("stdio_read_only_episode_reads_and_write_refusal"):
            host = Stdio(self.options.mcp, self.root, self.env, self.db, "read-only")
            try:
                bounded(host.tool("episode", {"db": "project", "action": "get", "episode_id": self.root_id}))
                for arguments in (
                    {"action": "append", **authored("denied")},
                    {"action": "revise", "episode_id": self.root_id, **self.revision_input},
                ):
                    denial = host.tool("episode", {"db": "project", **arguments}, ok=False)
                    need("capability" in denial.lower(), f"read-only refusal was not authority based: {denial}")
            finally:
                host.stop()

        with self.check("http_native_bridge_direct_owner_episode_parity") as evidence:
            host = Host(self.options.mcp, self.root, "episode-http", ["--capability-profile", "operator",
                        "--db", f"project={self.db}"], self.env)
            try:
                with McpClient(host.url, timeout=30) as client:
                    need(one_db(client, "project") == self.db_id, "upgrade or episode writes changed database identity")
                    got = bounded(client.call_tool("episode", {"db": "project", "action": "get",
                        "episode_id": self.root_id, "body": True}))
                    need(identity(got)[1] == self.latest_id and got["body"] == self.revision_input["body"],
                         "HTTP native bridge get differs from CLI")
                    context = mixed_context(client.call_tool("recall_context", {
                        "db": "project", "text": "violet lantern amber", "k": 8, "depth": 2}),
                        self.root_id, self.latest_id, self.first_id, {self.semantic_id, self.lesson_id})
                    for lane in ("core", "primary", "expansions", "episodes", "episodic_retrieval"):
                        need(context[lane] == self.stdio_context[lane], f"HTTP/stdio mixed context differs in {lane}")
                    # Exercise the actual hook boundary against native output,
                    # not a hand-authored shape which might miss serde drift.
                    from hook_recall import _candidates, _card, EPISODE_FIELDS, MAX_CARDS_BYTES
                    cards = []
                    for candidate in _candidates(context):
                        node = client.call_tool("get", {"db": "project", "id": candidate["id"],
                                                       "body": False, "edges": False})
                        card = _card(node, candidate["id"], candidate)
                        need(card is not None, "native hook candidate disappeared")
                        if card["kind"] == "episode":
                            need(all(card[key] == candidate[key] for key in EPISODE_FIELDS),
                                 "hook dropped native episode identity/time fields")
                        cards.append(card)
                    need({card["kind"] for card in cards} == {"semantic", "episode"}
                         and len(json.dumps(cards, ensure_ascii=False, separators=(",", ":")).encode()) <= MAX_CARDS_BYTES,
                         "native mixed hook card presentation lost a kind or exceeded its shared budget")
                    evidence["mixed_context"] = "HTTP/stdio lane parity; native get to Python hook _card"
                    escaped_body = '\u0001\u0002\\"雪' * 2000  # 14 KiB body, >32 KiB JSON.
                    payload = {"action": "append", **authored("http-first", body=escaped_body)}
                    prepared = client.prepare_episode(payload)
                    need(prepared.get("is_mutation") is True and prepared.get("payload") == payload,
                         "native episode preparation changed the request")
                    made = bounded(client.episode_verified("project", payload))
                    need(made.get("readback_status") == "verified", "native verified append lacked proof")
                    readback = bounded(client.call_tool("episode", {"db": "project", "action": "get",
                        "episode_id": identity(made)[0], "edition_id": identity(made)[1], "body": True}))
                    need(readback.get("body") and payload["body"].startswith(readback["body"])
                         and readback.get("summary") == payload["summary"]
                         and readback.get("body_range", {}).get("has_more") is True,
                         "escape-expanded episode did not yield a bounded first body chunk")
                    replay = bounded(client.episode_verified("project", payload))
                    need(identity(replay) == identity(made) and replay.get("replayed") is True
                         and replay.get("readback_status") == "verified", "HTTP verified retry failed")
                    edit_payload = {"action": "revise", "episode_id": identity(made)[0],
                        **authored("http-revision", expected_edition_id=identity(made)[1],
                            reason="Verify the native editorial readback.", body="The corrected HTTP scene.")}
                    edited = bounded(client.episode_verified("project", edit_payload))
                    need(edited.get("revision") == 1 and edited.get("readback_status") == "verified",
                         "native verified revision lacked proof")
                    old_retry = bounded(client.episode_verified("project", payload))
                    need(identity(old_retry) == identity(made) and old_retry.get("replayed") is True
                         and old_retry.get("readback_status") == "verified",
                         "native old-edition retry lost immutable readback")
                    conflict = expect_http_error(client, "episode", {"db": "project", "action": "append",
                        **{**payload, "summary": "Conflicting account"}})
                    need("conflict" in conflict.lower(), "HTTP changed request did not conflict")
                    for anchor in (self.lesson_id, self.first_id):
                        page = bounded(client.call_tool("episode", {"db": "project", "action": "references", "anchor": anchor}))
                        need(has_reference(page, self.lesson_id, self.first_id), "HTTP reference direction lost")
                    remote = subprocess.run([str(self.options.mnemed), "--remote", host.url,
                        "--remote-db", "project", "--json", "episode", "get", self.root_id, "--body"],
                        cwd=self.root, env=self.env, text=True, capture_output=True, timeout=90)
                    need(remote.returncode == 0, f"remote CLI episode get failed: {remote.stderr[-1000:]}")
                    remote_got = bounded(json.loads(remote.stdout))
                    need(identity(remote_got)[1] == self.latest_id and remote_got["body"] == self.revision_input["body"],
                         "remote CLI episode result differs from direct owner")
                    evidence["readback"] = "native episode_verified: escape-expanded 14 KiB append, revision, and historic append retry"
                    evidence["escaped_body_bytes"] = len(escaped_body.encode())
                    evidence["remote_cli"] = "direct-owner HTTP episode get"
            finally:
                host.stop()

    def run(self):
        self.upgrade_and_fence()
        self.cli_roundtrip()
        self.mcp_roundtrip()
        with self.check("closed_owner_source_unchanged_and_successor_reopens"):
            source_hash = self.receipt["checks"][0]["source_sha256_before"]
            need(canonical_hash(self.source) == source_hash, "later smoke steps changed predecessor source")
            got = self.episode("get", self.root_id, "--edition-id", self.first_id, "--body")
            need(got.get("body") == self.first_input["body"], "reopen lost original edition body")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ("mnemed", "mcp", "old-mnemed", "old-mcp", "output"):
        parser.add_argument("--" + flag, required=True, type=Path)
    options = parser.parse_args()
    binaries = {name: getattr(options, name.replace("-", "_")).resolve()
                for name in ("mnemed", "mcp", "old-mnemed", "old-mcp")}
    for name, path in binaries.items():
        need(path.is_file() and os.access(path, os.X_OK), f"missing {name} executable: {path}")
        setattr(options, name.replace("-", "_"), path)
    output = options.output.resolve()
    need(output not in binaries.values(), "receipt must not overwrite a binary")
    output.parent.mkdir(parents=True, exist_ok=True)
    receipt = {"schema": "mneme.episodic-installed-smoke.v1", "status": "running",
               "scope": "disposable direct-owner installed-artifact smoke, not a benchmark or exhaustive gate suite",
               "harness": artifact(Path(__file__).resolve()),
               "artifacts": {name: artifact(path) for name, path in binaries.items()},
               "checks": []}
    start = time.monotonic()
    try:
        with tempfile.TemporaryDirectory(prefix="mneme-episode-smoke-") as dirname:
            root = Path(dirname)
            try:
                Smoke(options, root, receipt).run()
            except Exception:
                receipt["logs"] = {path.name: path.read_text(errors="replace")[-6000:]
                                   for path in root.glob("*.log")}
                raise
        receipt["status"] = "passed"
    except Exception as error:
        receipt.update(status="failed", error=str(error)[-4000:])
        raise
    finally:
        receipt["duration_ms"] = round((time.monotonic() - start) * 1000)
        output.write_text(json.dumps(receipt, sort_keys=True, indent=2) + "\n", encoding="utf-8")
        print(json.dumps({"receipt": str(output), "status": receipt["status"],
                          "passed": sum(check["status"] == "passed" for check in receipt["checks"])}, sort_keys=True))


if __name__ == "__main__":
    main()

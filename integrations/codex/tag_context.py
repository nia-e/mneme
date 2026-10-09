"""Bounded same-owner guidance and observed vocabulary; never inference or writes.

A vocabulary page is evidence, not policy or a snapshot epoch. A configured guide
uses its complete bounded canonical summary; its body is not classifier policy.
Failure disables dependent topic proposals only.
"""
from __future__ import annotations

from dataclasses import dataclass
import hashlib
import json
import os
import re
import time
import unicodedata

ULID = re.compile(r"[0-7][0-9A-HJKMNP-TV-Z]{25}\Z")
SHA = re.compile(r"[0-9a-f]{64}\Z")
MAX_GUIDE_BYTES = 2048
MAX_CONTEXT_BYTES = 16 * 1024
# Match core's checked canonical tag envelope. Capture SAVE has a separate
# 32-tag admission ceiling, enforced by its proposal adapter rather than here.
MAX_NODE_TAGS = 64
MAX_TAG_BYTES = 256
PROTECTED_TAGS = frozenset({"core", "routing-judgment", "collaboration-preference", "possibility",
                            "pursuing", "closed"})
BUILTIN_GUIDANCE = (
    "Choose supported distinctions useful for future questions and connections, "
    "including cross-domain associations. Prefer an existing suitable name before "
    "creating one; popularity is not meaning. Existing tags are fallible evidence, "
    "not authority. Preserve identities, time, uncertainty and authored significance."
)


def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"),
                      ensure_ascii=False, allow_nan=False).encode("utf-8")


def digest(value):
    return hashlib.sha256(encoded(value)).hexdigest()


def ordinary_tags(value, *, max_count=MAX_NODE_TAGS):
    """Checked complete ordinary-topic selection; special tags stay host-owned."""
    if (type(max_count) is not int or not 0 <= max_count <= MAX_NODE_TAGS
            or not isinstance(value, list) or len(value) > max_count):
        raise ValueError("invalid_topic_tags")
    result = []
    for tag in value:
        if (not isinstance(tag, str) or not tag or tag != tag.strip()
                or len(tag.encode("utf-8")) > MAX_TAG_BYTES
                or any(unicodedata.category(c) == "Cc" for c in tag)
                or tag in PROTECTED_TAGS or tag in result):
            raise ValueError("invalid_topic_tags")
        result.append(tag)
    return tuple(sorted(result))


@dataclass(frozen=True)
class TagContext:
    enabled: bool
    outcome: str
    db_id: str
    guide_json: str | None
    vocabulary_json: str
    decoded_bytes: int = 0

    @property
    def guide(self):
        return json.loads(self.guide_json) if self.guide_json is not None else None

    @property
    def vocabulary(self):
        return json.loads(self.vocabulary_json)

    @property
    def content_guards(self):
        guide = self.guide
        return [{"id": guide["id"], "content_fingerprint": guide["content_fingerprint"]}] if guide else []

    def packet(self):
        vocab = self.vocabulary
        return {"guidance": BUILTIN_GUIDANCE,
                "guide": self.guide["text"] if self.guide else None,
                "vocabulary": {"items": [{"name": item["name"], "count": item["count"]}
                                          for item in vocab["items"]],
                               "partial": vocab["partial"],
                               "coverage": {"semantic_only": True, "snapshot": False,
                                            "lookup_prefixes_omitted": vocab["coverage"].get("lookup_prefixes_omitted", 0),
                                            "seed_tags_omitted": vocab["coverage"].get("seed_tags_omitted", 0),
                                            "cue_truncated": vocab["coverage"].get("cue_truncated", False)}}}

    def snapshot(self):
        guide = self.guide
        identity = ({key: guide[key] for key in ("id", "content_fingerprint", "text_sha256")}
                    if guide else None)
        return {"db_id": self.db_id, "guide": identity,
                "guide_summary_sha256": guide["text_sha256"] if guide else None,
                "guide_revision_sha256": digest({"builtin": BUILTIN_GUIDANCE,
                    "guide": {"id": guide["id"], "text_sha256": guide["text_sha256"]} if guide else None}),
                "vocabulary_sha256": digest(self.vocabulary),
                "vocabulary_partial": self.vocabulary.get("partial", True), "outcome": self.outcome}


def disabled(db_id, outcome="disabled", decoded_bytes=0):
    return TagContext(False, outcome, db_id, None,
                      encoded({"items": [], "partial": True, "coverage": {}}).decode(), decoded_bytes)


def _validate_context(context, expected_db_id=None):
    if (not isinstance(context, TagContext) or type(context.enabled) is not bool
            or not isinstance(context.db_id, str) or not ULID.fullmatch(context.db_id)
            or expected_db_id is not None and context.db_id != expected_db_id
            or type(context.decoded_bytes) is not int or context.decoded_bytes < 0
            or not isinstance(context.vocabulary_json, str)
            or len(context.vocabulary_json.encode()) > MAX_CONTEXT_BYTES
            or context.guide_json is not None and (not isinstance(context.guide_json, str)
                                                  or len(context.guide_json.encode()) > MAX_CONTEXT_BYTES)):
        raise ValueError("invalid_tag_context")
    if len(encoded(context.packet())) > MAX_CONTEXT_BYTES:
        raise ValueError("invalid_tag_context")
    guide = context.guide
    if guide is not None:
        if (not isinstance(guide, dict) or set(guide) != {"id", "content_fingerprint", "text_sha256", "text"}
                or not isinstance(guide["id"], str) or not ULID.fullmatch(guide["id"])
                or not isinstance(guide["content_fingerprint"], str) or not SHA.fullmatch(guide["content_fingerprint"])
                or not isinstance(guide["text"], str) or not guide["text"].strip()
                or len(guide["text"].encode()) > MAX_GUIDE_BYTES
                or any(unicodedata.category(c) == "Cc" and c not in "\n\r\t" for c in guide["text"])
                or hashlib.sha256(guide["text"].encode()).hexdigest() != guide["text_sha256"]):
            raise ValueError("invalid_tag_context")
    vocab = context.vocabulary
    if (not isinstance(vocab, dict) or not isinstance(vocab.get("items"), list)
            or type(vocab.get("partial")) is not bool
            or not isinstance(vocab.get("coverage"), dict)):
        raise ValueError("invalid_tag_context")
    names = set()
    for item in vocab["items"]:
        if not isinstance(item, dict) or set(item) != {"name", "count", "examples"}:
            raise ValueError("invalid_tag_context")
        name = item["name"]
        # Observed special tags may be visible but cannot be selected as topics.
        if (not isinstance(name, str) or not name or name != name.strip() or len(name.encode()) > MAX_TAG_BYTES
                or any(unicodedata.category(c) == "Cc" for c in name) or name in names):
            raise ValueError("invalid_tag_context")
        names.add(name)
        count = item["count"]
        if (not isinstance(count, dict) or count.get("status") not in ("exact", "lower_bound", "unavailable")
                or set(count) != ({"status"} if count["status"] == "unavailable" else {"status", "value"})
                or count["status"] != "unavailable" and (type(count["value"]) is not int or count["value"] < 0)
                or not isinstance(item["examples"], list) or len(item["examples"]) > 16
                or any(not isinstance(i, str) or not ULID.fullmatch(i) for i in item["examples"])):
            raise ValueError("invalid_tag_context")
    return context


def validate_context(context, expected_db_id=None):
    try:
        return _validate_context(context, expected_db_id)
    except (ValueError, TypeError, KeyError, AttributeError, UnicodeError, RecursionError) as error:
        raise ValueError("invalid_tag_context") from error


def lookup_prefixes(cue, seed_tags=()):
    """Lexical discovery hints, never synonym guesses or semantic judgments."""
    if (not isinstance(cue, str) or len(cue.encode()) > 8192
            or not isinstance(seed_tags, (list, tuple)) or len(seed_tags) > MAX_CONTEXT_BYTES // 4):
        raise ValueError("invalid_tag_lookup_cue")
    result = []
    for tag in seed_tags:
        # Existing special names are discoverable evidence, not selectable topics.
        if (not isinstance(tag, str) or not tag or tag != tag.strip() or len(tag.encode()) > MAX_TAG_BYTES
                or any(unicodedata.category(c) == "Cc" for c in tag)):
            raise ValueError("invalid_tag_lookup_cue")
        if tag not in result:
            result.append(tag)
    stop = {"the", "and", "with", "this", "that", "from", "have", "about", "could", "would",
            "should", "into", "what", "when", "where", "there", "which", "your", "their",
            "please", "using", "want", "need", "memory", "tags", "tag", "remember"}
    for word in re.findall(r"\w+(?:[-.:]\w+)*", cue):
        word = word.lower()
        if 3 <= len(word.encode()) <= MAX_TAG_BYTES and word not in stop and word not in result:
            result.append(word)
    return result


def collect(config, *, expected_db_id, timeout, max_bytes=MAX_CONTEXT_BYTES, cue="", seed_tags=()):
    """Directed lexical pages + fallback within one aggregate read/time envelope.

    Prefix selection is deterministic and fallible. Partial pages and unqueried
    prefixes stay visible; neither constitutes proof that an existing name is absent.
    """
    if config.get("tag_stewardship") is not True:
        return disabled(expected_db_id)
    if (not isinstance(expected_db_id, str) or not ULID.fullmatch(expected_db_id)
            or type(timeout) not in (int, float) or not 0 < timeout <= 30
            or type(max_bytes) is not int or max_bytes <= 0):
        return disabled(expected_db_id, "invalid_tag_context")
    max_bytes = min(max_bytes, MAX_CONTEXT_BYTES)
    from mcp_client import McpClient
    from target_policy import policy_for, target_kwargs
    deadline = time.monotonic() + timeout
    consumed = 0
    client = None
    try:
        target = target_kwargs(config)
        policy, service = policy_for(config["service_config"], config["project_root"], **target)
        alias = policy.db_alias
        token = os.environ.get(service.token_env) if service.token_env else None
        if service.token_env and not token:
            raise ValueError("native_unavailable")
        client = McpClient(service.url, token=token, timeout=min(30, timeout))
        client.connect()

        def call(name, args):
            nonlocal consumed
            left = deadline - time.monotonic()
            if left <= 0:
                raise ValueError("tag_context_deadline")
            client.timeout = min(30, left)
            value = client.call_tool(name, {"db": alias, "expected_db_id": expected_db_id, **args})
            consumed += len(encoded(value))
            if (consumed > max_bytes or not isinstance(value, dict)
                    or value.get("db") != alias or value.get("db_id") != expected_db_id):
                raise ValueError("tag_context_unavailable")
            return value

        guide = None
        guide_id = config.get("tag_guide_id")
        if guide_id is not None:
            if not isinstance(guide_id, str) or not ULID.fullmatch(guide_id):
                raise ValueError("invalid_guide")
            node = call("get", {"id": guide_id})
            text = node.get("summary")
            if (node.get("id") != guide_id or node.get("memory_kind") != {"kind": "semantic"}
                    or node.get("content_fingerprint_codec") != "mneme.routing-content.v2"
                    or not isinstance(node.get("content_fingerprint"), str)
                    or not SHA.fullmatch(node["content_fingerprint"])
                    or not isinstance(text, str) or node.get("summary_truncated") is not False):
                raise ValueError("invalid_guide")
            guide = {"id": guide_id, "content_fingerprint": node["content_fingerprint"],
                     "text_sha256": hashlib.sha256(text.encode()).hexdigest(), "text": text}
        if not isinstance(cue, str):
            raise ValueError("invalid_tag_lookup_cue")
        cue_bytes = cue.encode()
        if not isinstance(seed_tags, (list, tuple)):
            raise ValueError("invalid_tag_lookup_cue")
        # Batches can contain more distinct existing tags than one native node.
        # Admit whole lexical hints in byte room; omissions are not bad tags.
        admitted_seeds, seed_bytes = [], 0
        for seed in seed_tags[:MAX_CONTEXT_BYTES // 4]:
            if not isinstance(seed, str):
                raise ValueError("invalid_tag_lookup_cue")
            size = len(seed.encode()) + 3
            if seed_bytes + size > 8192:
                break
            admitted_seeds.append(seed)
            seed_bytes += size
        seed_omitted = len(seed_tags) - len(admitted_seeds)
        prefixes = lookup_prefixes(cue_bytes[:8192].decode("utf-8", "ignore"), admitted_seeds)
        # Reserve a page's small framing/work room per lookup. The allowance,
        # not vocabulary size, determines how many hints can be followed.
        slots = max(1, (max_bytes - consumed) // 2048)
        chosen = prefixes[:max(0, slots - 1)] + [None]
        items, queries, partial = {}, [], bool(seed_omitted) or len(prefixes) > max(0, slots - 1)
        for index, prefix in enumerate(chosen):
            room = max_bytes - consumed
            if room < 512 or deadline <= time.monotonic():
                partial = True
                break
            page_room = room // (len(chosen) - index)
            request = {"kind": "tags", "status": "all", "limit": min(64, max(1, page_room // 512))}
            if prefix is not None:
                request["prefix"] = prefix
            page = call("list", request)
            if (page.get("kind") != "tags" or type(page.get("partial")) is not bool
                    or not isinstance(page.get("items"), list) or len(page["items"]) > request["limit"]
                    or type(page.get("has_more")) is not bool
                    or page["has_more"] and not isinstance(page.get("next_cursor"), str)
                    or not page["has_more"] and page.get("next_cursor") is not None):
                raise ValueError("invalid_vocabulary")
            page_vocab = {"items": page.get("items"), "partial": page["partial"] or page["has_more"],
                          "coverage": page.get("coverage")}
            if (not isinstance(page_vocab["coverage"], dict)
                    or page_vocab["coverage"].get("semantic_only") is not True
                    or page_vocab["coverage"].get("snapshot") is not False):
                raise ValueError("invalid_vocabulary")
            validate_context(TagContext(True, "ok", expected_db_id, None, encoded(page_vocab).decode()))
            for item in page_vocab["items"]:
                if prefix is not None and not item["name"].startswith(prefix):
                    raise ValueError("invalid_vocabulary")
                items.setdefault(item["name"], item)
            queries.append({"prefix": prefix, "partial": page_vocab["partial"],
                            "returned": len(page_vocab["items"]), "work": page_vocab["coverage"]})
            partial |= page_vocab["partial"]
        vocab = {"items": list(items.values()), "partial": partial,
                 "coverage": {"semantic_only": True, "snapshot": False, "queries": queries,
                              "cue_truncated": len(cue_bytes) > 8192,
                              "seed_tags_omitted": seed_omitted,
                              "lookup_prefixes_omitted": len(prefixes) - sum(q["prefix"] is not None for q in queries)}}
        if not queries:
            raise ValueError("tag_context_unavailable")
        context = TagContext(True, "ok", expected_db_id,
                             encoded(guide).decode() if guide else None, encoded(vocab).decode(), consumed)
        return validate_context(context, expected_db_id)
    except Exception:
        return disabled(expected_db_id, "tag_context_unavailable", consumed)
    finally:
        if client is not None:
            client.timeout = .1
            try:
                client.close()
            except Exception:
                pass

#!/usr/bin/env python3
"""Independent byte oracle for the managed-snapshot MNF v1 container.

This is review tooling, not production code. Rust must pin literal known-answer
values and implement the grammar directly rather than invoking this module.
"""

from __future__ import annotations

import hashlib
import json
import struct
from dataclasses import dataclass
from typing import Sequence


POLICY_DOMAIN = b"mneme-managed-snapshot-framing-policy-transcript-v1\0"
FRAME_DOMAIN = b"mneme-managed-snapshot-frame-v1\0"
INVENTORY_DOMAIN = b"mneme-managed-snapshot-frame-inventory-v1\0"
BODY_CONTENT_DOMAIN = b"mneme-managed-snapshot-body-content-v1\0"
BODY_LOCAL_MAP_DOMAIN = b"mneme-managed-snapshot-body-local-map-v1\0"
BODY_EXTERNAL_MAP_DOMAIN = b"mneme-managed-snapshot-body-external-map-v1\0"
MAGIC = b"MNEMEMNF"
FOOTER_MAGIC = b"MNFEND\0\1"
FORMAT_VERSION = 1
RECORDS = 1
BODIES = 2
PREAMBLE_BYTES = 44
FOOTER_BYTES = 56
FRAME_FIXED_AFTER_LENGTH_BYTES = 51
MAX_ITEMS_PER_FRAME = 256
MAX_RECORD_PAYLOAD_BYTES = 2_097_152
MAX_BODY_PAYLOAD_BYTES = 16_777_216
MAX_RECORD_FRAMES = 65_551
MAX_BODY_FRAMES = 131_073
MAX_RECORD_STREAM_BYTES = 137_438_953_472
MAX_BODY_STREAM_BYTES = 549_755_813_888


# Exact finite ASCII transcript. The fingerprint excludes only its own output.
POLICY_LINES = (
    "mneme.managed-snapshot.mnf-policy.v1",
    "integers=u16,u32,u64 unsigned big-endian",
    "preamble=magic:MNEMEMNF|version:u16=1|stream:u8=1-records,2-bodies|flags:u8=0|policy:sha256",
    "frame=encoded-length:u32=51+payload|kind:u8|relation:u16|page:u64|items:u32|payload-length:u32|digest:sha256|payload",
    "frame-digest=sha256(domain:mneme-managed-snapshot-frame-v1\\0|stream|kind|relation|page|items|payload-length|payload)",
    "footer=magic:4d4e46454e440001|frames:u64|items:u64|ordered-frame-digest:sha256|eof",
    "inventory=sha256(domain:mneme-managed-snapshot-frame-inventory-v1\\0|stream|each:01,kind,relation,page,items,encoded-length,digest|ff,frames,items)",
    "records=kind:1|relation:1..16|page:zero-based-per-relation|items:1..256|payload:managed-snapshot-page-v1-compact-json",
    "records-order=relation-ascending|no-cross-relation-frame|greedy-256-except-relation-terminal|one-global-after-next-chain",
    "record-json=order:head,after,limit,records,has_more,next,page_digest|head:state,database_id|cursor:head_binding,after|record:position,digest",
    "bodies=relation:0|kind:2-census,3-local-map,4-external-marker|kind-ascending|page:zero-based-per-kind|items:1..256|greedy-256-except-section-terminal",
    "body-json=object-order:schema,section,page_ordinal,items|schema:mneme.managed-snapshot.body-page.v1|sections:census,local_map,external_markers",
    "json=compact-ascii,no-whitespace,no-newline|integers:shortest-decimal|bool:true,false|null:null|unknown-or-duplicate-field:reject|max-depth:4",
    "hex=lowercase-even|declared-byte-length-times-two|sha256:64-hex",
    "census-item-order=name_bytes_length,name_bytes_hex,bytes,sha256,device,inode,mode,uid,gid,link_count,mtime_seconds,mtime_nanoseconds,ctime_seconds,ctime_nanoseconds",
    "census-types=name-length:u16<=255|bytes:u64|device:u64|inode:u64|mode:u32-full-st-mode|uid:u32|gid:u32|link-count:u64|seconds:i64|nanoseconds:u32<=999999999",
    "local-item-order=body_ref_bytes_length,body_ref_bytes_hex,entry_name_bytes_length,entry_name_bytes_hex,occurrences",
    "external-item-order=body_ref_bytes_length,body_ref_bytes_hex,occurrences",
    "map-types=body-ref-length:u16<=16384|entry-name-length:u16<=255|occurrences:u64-positive",
    "body-order=census-name-strict|local-ref-strict|external-ref-strict|local-external-disjoint|frame-items-equal-json-items",
    "limits=record-payload:2097152|body-payload:16777216|record-frames:65551|body-frames:131073|record-stream:137438953472|body-stream:549755813888|items-per-frame:256",
    "parser=header-authenticated-file-length-first|frames-only-between-offsets-44-and-len-minus-56|exact-footer-cursor|no-trailing-bytes|unknown-version-kind-flags:reject|lengths-and-overflow-before-allocation|body-item-visitor-stops-at-min(item-count-plus-one,257)",
)


def u16(value: int) -> bytes:
    return struct.pack(">H", value)


def u32(value: int) -> bytes:
    return struct.pack(">I", value)


def u64(value: int) -> bytes:
    return struct.pack(">Q", value)


def sha256(value: bytes) -> bytes:
    return hashlib.sha256(value).digest()


def policy_transcript() -> bytes:
    return ("\n".join(POLICY_LINES) + "\n").encode("ascii")


def policy_fingerprint() -> bytes:
    transcript = policy_transcript()
    return sha256(POLICY_DOMAIN + u32(len(transcript)) + transcript)


def preamble(stream_kind: int) -> bytes:
    if stream_kind not in (RECORDS, BODIES):
        raise ValueError("unknown stream kind")
    encoded = MAGIC + u16(FORMAT_VERSION) + bytes((stream_kind, 0)) + policy_fingerprint()
    if len(encoded) != PREAMBLE_BYTES:
        raise AssertionError("preamble size drift")
    return encoded


@dataclass(frozen=True)
class Frame:
    stream_kind: int
    frame_kind: int
    relation_ordinal: int
    page_ordinal: int
    item_count: int
    payload: bytes

    def __post_init__(self) -> None:
        if self.stream_kind == RECORDS:
            if self.frame_kind != 1 or not 1 <= self.relation_ordinal <= 16:
                raise ValueError("invalid record frame kind or relation")
            payload_cap = MAX_RECORD_PAYLOAD_BYTES
        elif self.stream_kind == BODIES:
            if self.frame_kind not in (2, 3, 4) or self.relation_ordinal != 0:
                raise ValueError("invalid body frame kind or relation")
            payload_cap = MAX_BODY_PAYLOAD_BYTES
        else:
            raise ValueError("unknown stream kind")
        if not 0 <= self.page_ordinal < (1 << 64):
            raise ValueError("page ordinal out of range")
        if not 1 <= self.item_count <= MAX_ITEMS_PER_FRAME:
            raise ValueError("item count out of range")
        if len(self.payload) > payload_cap:
            raise ValueError("payload exceeds stream cap")

    def digest(self) -> bytes:
        return sha256(
            FRAME_DOMAIN
            + bytes((self.stream_kind, self.frame_kind))
            + u16(self.relation_ordinal)
            + u64(self.page_ordinal)
            + u32(self.item_count)
            + u32(len(self.payload))
            + self.payload
        )

    def encoded_length(self) -> int:
        return FRAME_FIXED_AFTER_LENGTH_BYTES + len(self.payload)

    def encode(self) -> bytes:
        return (
            u32(self.encoded_length())
            + bytes((self.frame_kind,))
            + u16(self.relation_ordinal)
            + u64(self.page_ordinal)
            + u32(self.item_count)
            + u32(len(self.payload))
            + self.digest()
            + self.payload
        )


def inventory_digest(stream_kind: int, frames: Sequence[Frame]) -> bytes:
    digest = hashlib.sha256()
    digest.update(INVENTORY_DOMAIN)
    digest.update(bytes((stream_kind,)))
    total_items = 0
    for frame in frames:
        if frame.stream_kind != stream_kind:
            raise ValueError("frame belongs to another stream")
        digest.update(b"\x01")
        digest.update(bytes((frame.frame_kind,)))
        digest.update(u16(frame.relation_ordinal))
        digest.update(u64(frame.page_ordinal))
        digest.update(u32(frame.item_count))
        digest.update(u32(frame.encoded_length()))
        digest.update(frame.digest())
        total_items += frame.item_count
    digest.update(b"\xff")
    digest.update(u64(len(frames)))
    digest.update(u64(total_items))
    return digest.digest()


def _validate_order(stream_kind: int, frames: Sequence[Frame]) -> None:
    maximum = MAX_RECORD_FRAMES if stream_kind == RECORDS else MAX_BODY_FRAMES
    if len(frames) > maximum:
        raise ValueError("frame count exceeds stream cap")
    previous_group = 0
    expected_page = 0
    for index, frame in enumerate(frames):
        if frame.stream_kind != stream_kind:
            raise ValueError("frame belongs to another stream")
        group = frame.relation_ordinal if stream_kind == RECORDS else frame.frame_kind
        if group < previous_group:
            raise ValueError("frame group order decreased")
        if group != previous_group:
            previous_group = group
            expected_page = 0
        if frame.page_ordinal != expected_page:
            raise ValueError("non-consecutive page ordinal")
        if (
            index + 1 < len(frames)
            and (
                frames[index + 1].relation_ordinal
                if stream_kind == RECORDS
                else frames[index + 1].frame_kind
            )
            == group
            and frame.item_count != MAX_ITEMS_PER_FRAME
        ):
            raise ValueError("nonterminal group frame is not greedily full")
        expected_page += 1


def footer(stream_kind: int, frames: Sequence[Frame]) -> bytes:
    total_items = sum(frame.item_count for frame in frames)
    encoded = (
        FOOTER_MAGIC
        + u64(len(frames))
        + u64(total_items)
        + inventory_digest(stream_kind, frames)
    )
    if len(encoded) != FOOTER_BYTES:
        raise AssertionError("footer size drift")
    return encoded


def stream(stream_kind: int, frames: Sequence[Frame]) -> bytes:
    _validate_order(stream_kind, frames)
    encoded = preamble(stream_kind) + b"".join(frame.encode() for frame in frames)
    encoded += footer(stream_kind, frames)
    maximum = (
        MAX_RECORD_STREAM_BYTES if stream_kind == RECORDS else MAX_BODY_STREAM_BYTES
    )
    if len(encoded) > maximum:
        raise ValueError("stream exceeds byte cap")
    return encoded


def compact_json(value: object) -> bytes:
    return json.dumps(
        value, ensure_ascii=True, allow_nan=False, separators=(",", ":")
    ).encode("ascii")


def body_content_digest(
    root_present: bool, entries: Sequence[tuple[bytes, int, bytes]]
) -> bytes:
    digest = hashlib.sha256()
    digest.update(BODY_CONTENT_DOMAIN)
    digest.update(bytes((int(root_present),)))
    previous = None
    aggregate = 0
    for name, byte_count, content_sha256 in entries:
        if not name or len(name) > 255 or previous is not None and name <= previous:
            raise ValueError("body census names must be unique and strictly ordered")
        if not 0 <= byte_count < (1 << 64) or len(content_sha256) != 32:
            raise ValueError("invalid body census entry")
        digest.update(b"\x01")
        digest.update(u16(len(name)))
        digest.update(name)
        digest.update(u64(byte_count))
        digest.update(content_sha256)
        aggregate += byte_count
        if aggregate >= 1 << 64:
            raise ValueError("body byte total overflows u64")
        previous = name
    if not root_present and entries:
        raise ValueError("absent body root cannot have census entries")
    digest.update(b"\xff")
    digest.update(u64(len(entries)))
    digest.update(u64(aggregate))
    return digest.digest()


def body_map_digest(
    domain: bytes, mappings: Sequence[tuple[bytes, bytes | None, int]]
) -> bytes:
    if domain not in (BODY_LOCAL_MAP_DOMAIN, BODY_EXTERNAL_MAP_DOMAIN):
        raise ValueError("unknown body-map digest domain")
    digest = hashlib.sha256()
    digest.update(domain)
    previous = None
    occurrences = 0
    for body_ref, entry_name, count in mappings:
        if (
            not body_ref
            or len(body_ref) > 16_384
            or previous is not None
            and body_ref <= previous
            or not 0 < count < (1 << 64)
        ):
            raise ValueError("invalid or unordered body mapping")
        body_ref.decode("utf-8")
        digest.update(b"\x01")
        digest.update(u16(len(body_ref)))
        digest.update(body_ref)
        if domain == BODY_LOCAL_MAP_DOMAIN:
            if entry_name is None:
                raise ValueError("local mapping requires a census entry name")
            if not entry_name or len(entry_name) > 255:
                raise ValueError("invalid body entry name")
            digest.update(u16(len(entry_name)))
            digest.update(entry_name)
        elif entry_name is not None:
            raise ValueError("external marker cannot name a census entry")
        digest.update(u64(count))
        occurrences += count
        if occurrences >= 1 << 64:
            raise ValueError("body occurrence total overflows u64")
        previous = body_ref
    digest.update(b"\xff")
    digest.update(u64(len(mappings)))
    digest.update(u64(occurrences))
    return digest.digest()


def body_payload_fixtures() -> dict[str, bytes]:
    census = {
        "schema": "mneme.managed-snapshot.body-page.v1",
        "section": "census",
        "page_ordinal": 0,
        "items": [
            {
                "name_bytes_length": 1,
                "name_bytes_hex": "78",
                "bytes": 0,
                "sha256": hashlib.sha256(b"").hexdigest(),
                "device": 1,
                "inode": 2,
                "mode": 33152,
                "uid": 3,
                "gid": 4,
                "link_count": 1,
                "mtime_seconds": 0,
                "mtime_nanoseconds": 0,
                "ctime_seconds": 0,
                "ctime_nanoseconds": 0,
            }
        ],
    }
    local_ref = b"fs:///tmp/x"
    local = {
        "schema": "mneme.managed-snapshot.body-page.v1",
        "section": "local_map",
        "page_ordinal": 0,
        "items": [
            {
                "body_ref_bytes_length": len(local_ref),
                "body_ref_bytes_hex": local_ref.hex(),
                "entry_name_bytes_length": 1,
                "entry_name_bytes_hex": "78",
                "occurrences": 1,
            }
        ],
    }
    external_ref = b"inline://x"
    external = {
        "schema": "mneme.managed-snapshot.body-page.v1",
        "section": "external_markers",
        "page_ordinal": 0,
        "items": [
            {
                "body_ref_bytes_length": len(external_ref),
                "body_ref_bytes_hex": external_ref.hex(),
                "occurrences": 1,
            }
        ],
    }
    return {
        "census": compact_json(census),
        "local_map": compact_json(local),
        "external_markers": compact_json(external),
    }


def fixtures() -> dict[str, str | int]:
    opaque_record = Frame(RECORDS, 1, 1, 0, 1, b"{}")
    body_payloads = body_payload_fixtures()
    census = Frame(BODIES, 2, 0, 0, 1, body_payloads["census"])
    local = Frame(BODIES, 3, 0, 0, 1, body_payloads["local_map"])
    external = Frame(BODIES, 4, 0, 0, 1, body_payloads["external_markers"])
    empty_records = stream(RECORDS, ())
    one_record = stream(RECORDS, (opaque_record,))
    empty_sha256 = hashlib.sha256(b"").digest()
    return {
        "policy_transcript_bytes": len(policy_transcript()),
        "policy_fingerprint": policy_fingerprint().hex(),
        "records_preamble": preamble(RECORDS).hex(),
        "bodies_preamble": preamble(BODIES).hex(),
        "empty_records_bytes": len(empty_records),
        "empty_records_sha256": sha256(empty_records).hex(),
        "opaque_record_frame_digest": opaque_record.digest().hex(),
        "opaque_record_inventory_digest": inventory_digest(RECORDS, (opaque_record,)).hex(),
        "opaque_record_stream_bytes": len(one_record),
        "opaque_record_stream_sha256": sha256(one_record).hex(),
        "census_payload_sha256": sha256(body_payloads["census"]).hex(),
        "census_frame_digest": census.digest().hex(),
        "local_payload_sha256": sha256(body_payloads["local_map"]).hex(),
        "local_frame_digest": local.digest().hex(),
        "external_payload_sha256": sha256(body_payloads["external_markers"]).hex(),
        "external_frame_digest": external.digest().hex(),
        "body_content_present_x_empty": body_content_digest(
            True, ((b"x", 0, empty_sha256),)
        ).hex(),
        "body_content_absent": body_content_digest(False, ()).hex(),
        "body_local_map_x": body_map_digest(
            BODY_LOCAL_MAP_DOMAIN, ((b"fs:///tmp/x", b"x", 1),)
        ).hex(),
        "body_external_map_x": body_map_digest(
            BODY_EXTERNAL_MAP_DOMAIN, ((b"inline://x", None, 1),)
        ).hex(),
    }


def main() -> None:
    print(json.dumps(fixtures(), sort_keys=True, indent=2))


if __name__ == "__main__":
    main()

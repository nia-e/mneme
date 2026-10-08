#!/usr/bin/env python3
"""Independent byte oracle for the managed-snapshot-v1 logical codec.

This is review tooling, not production code.  The Rust implementation must pin
literal known-answer values rather than importing or invoking this module.
"""

from __future__ import annotations

import hashlib
import json
import struct
from dataclasses import dataclass
from typing import Iterable, Sequence


RECORD_DOMAIN = b"mneme-managed-snapshot-record-v1\0"
RELATION_DOMAIN = b"mneme-managed-snapshot-relation-v1\0"
STORE_DOMAIN = b"mneme-managed-snapshot-store-v1\0"
MAX_RELATION_ORDINAL = 16
MAX_PRIMARY_KEY_BYTES = 2_376
MAX_POSITION_BYTES = 2_378
MAX_VALUE_TUPLE_BYTES = 2_097_152
MAX_VECTOR_DIMENSION = 4_096
MAX_RECORDS = 1 << 24

KEY_STRING = 1
KEY_I64 = 2
VALUE_NULL = 0
VALUE_STRING = 1
VALUE_BOOL = 2
VALUE_I64 = 3
VALUE_F32 = 4
VALUE_VEC_F32 = 5
NULLABLE_STRING = (VALUE_NULL, VALUE_STRING)

# Exact key/value column types for the frozen 16-base-relation vocabulary.
RELATION_SIGNATURES: dict[int, tuple[tuple[int, ...], tuple[int | tuple[int, ...], ...]]] = {
    1: ((KEY_STRING,), (VALUE_STRING,)),
    2: ((KEY_STRING,), (VALUE_STRING, VALUE_STRING)),
    3: ((KEY_STRING, KEY_STRING), ()),
    4: ((KEY_STRING, KEY_STRING, KEY_I64, KEY_STRING), ()),
    5: ((KEY_STRING,), (VALUE_STRING, VALUE_STRING)),
    6: ((KEY_STRING,), (VALUE_VEC_F32, VALUE_STRING)),
    7: ((KEY_STRING, KEY_STRING), (VALUE_F32, VALUE_STRING, VALUE_I64, VALUE_I64, VALUE_I64)),
    8: ((KEY_STRING, KEY_STRING), (VALUE_I64, VALUE_I64)),
    9: ((KEY_STRING, KEY_STRING), (VALUE_I64, VALUE_I64, VALUE_I64, NULLABLE_STRING)),
    10: ((KEY_STRING, KEY_STRING), (VALUE_I64, VALUE_I64, VALUE_I64, NULLABLE_STRING)),
    11: ((KEY_STRING, KEY_STRING), (VALUE_STRING, VALUE_STRING, VALUE_I64)),
    12: ((KEY_STRING, KEY_STRING), (VALUE_STRING, VALUE_STRING, VALUE_I64)),
    13: ((KEY_STRING, KEY_STRING, KEY_STRING), (VALUE_F32,)),
    14: ((KEY_STRING,), (VALUE_STRING, VALUE_I64)),
    15: ((KEY_STRING, KEY_I64, KEY_STRING), (VALUE_BOOL,)),
    16: ((KEY_STRING,), (VALUE_STRING,)),
}


def u16(value: int) -> bytes:
    return struct.pack(">H", value)


def u32(value: int) -> bytes:
    return struct.pack(">I", value)


def u64(value: int) -> bytes:
    return struct.pack(">Q", value)


def key_string(value: str) -> bytes:
    escaped = bytearray()
    for byte in value.encode("utf-8"):
        if byte == 0:
            escaped.extend((0, 0xFF))
        else:
            escaped.append(byte)
    return b"\x01" + bytes(escaped) + b"\x00\x00"


def key_i64(value: int) -> bytes:
    if not -(1 << 63) <= value < (1 << 63):
        raise ValueError("i64 out of range")
    unsigned = value & ((1 << 64) - 1)
    return b"\x02" + u64(unsigned ^ (1 << 63))


def _decode_key_string(encoded: bytes) -> bytes:
    if not encoded or encoded[0] != KEY_STRING:
        raise ValueError("wrong string-key tag")
    decoded = bytearray()
    cursor = 1
    while cursor < len(encoded):
        byte = encoded[cursor]
        cursor += 1
        if byte != 0:
            decoded.append(byte)
            continue
        if cursor == len(encoded):
            raise ValueError("truncated string-key escape")
        escaped = encoded[cursor]
        cursor += 1
        if escaped == 0:
            if cursor != len(encoded):
                raise ValueError("trailing bytes after string-key terminator")
            bytes(decoded).decode("utf-8")
            return bytes(decoded)
        if escaped == 0xFF:
            decoded.append(0)
            continue
        raise ValueError("invalid string-key escape")
    raise ValueError("missing string-key terminator")


def _validate_key_component(encoded: bytes, expected_tag: int) -> None:
    if not encoded or encoded[0] != expected_tag:
        raise ValueError("wrong key component type")
    if expected_tag == KEY_STRING:
        _decode_key_string(encoded)
    elif expected_tag == KEY_I64:
        if len(encoded) != 9:
            raise ValueError("integer key must contain exactly eight payload bytes")
    else:
        raise ValueError("unknown key component type")


def _validate_position(encoded: bytes) -> int:
    if len(encoded) < 3 or len(encoded) > MAX_POSITION_BYTES:
        raise ValueError("position length out of range")
    relation = int.from_bytes(encoded[:2], "big")
    signature = RELATION_SIGNATURES.get(relation)
    if signature is None:
        raise ValueError("relation ordinal out of range")
    expected_keys = signature[0]
    if encoded[2] != len(expected_keys):
        raise ValueError("wrong key arity")
    cursor = 3
    for expected_tag in expected_keys:
        if cursor == len(encoded) or encoded[cursor] != expected_tag:
            raise ValueError("wrong key component type")
        if expected_tag == KEY_I64:
            end = cursor + 9
            _validate_key_component(encoded[cursor:end], expected_tag)
            cursor = end
            continue
        end = cursor + 1
        while end < len(encoded):
            if encoded[end] != 0:
                end += 1
                continue
            if end + 1 >= len(encoded):
                raise ValueError("truncated string-key escape")
            if encoded[end + 1] == 0:
                end += 2
                break
            if encoded[end + 1] == 0xFF:
                end += 2
                continue
            raise ValueError("invalid string-key escape")
        _validate_key_component(encoded[cursor:end], expected_tag)
        cursor = end
    if cursor != len(encoded):
        raise ValueError("trailing position bytes")
    return relation


def position(relation: int, *components: bytes) -> bytes:
    signature = RELATION_SIGNATURES.get(relation)
    if signature is None:
        raise ValueError("relation ordinal out of range")
    expected_keys = signature[0]
    if len(components) != len(expected_keys):
        raise ValueError("wrong key arity")
    for encoded, expected_tag in zip(components, expected_keys):
        _validate_key_component(encoded, expected_tag)
    primary_key = bytes((len(components),)) + b"".join(components)
    if len(primary_key) > MAX_PRIMARY_KEY_BYTES:
        raise ValueError("primary key exceeds v1 cap")
    encoded = u16(relation) + primary_key
    _validate_position(encoded)
    return encoded


def component(tag: int, payload: bytes) -> bytes:
    if tag not in range(VALUE_NULL, VALUE_VEC_F32 + 1):
        raise ValueError("value tag out of range")
    encoded = bytes((tag,)) + u32(len(payload)) + payload
    _validate_value_component(encoded, tag)
    return encoded


def value_null() -> bytes:
    return component(0, b"")


def value_string(value: str) -> bytes:
    return component(1, value.encode("utf-8"))


def value_bool(value: bool) -> bytes:
    if not isinstance(value, bool):
        raise ValueError("bool constructor requires a bool")
    return component(2, bytes((int(value),)))


def value_i64(value: int) -> bytes:
    if not -(1 << 63) <= value < (1 << 63):
        raise ValueError("i64 out of range")
    unsigned = value & ((1 << 64) - 1)
    return component(3, u64(unsigned ^ (1 << 63)))


def value_f32_bits(bits: int) -> bytes:
    return component(4, u32(bits))


def value_vec_f32_bits(bits: Sequence[int]) -> bytes:
    if not 1 <= len(bits) <= MAX_VECTOR_DIMENSION:
        raise ValueError("vector dimension out of range")
    payload = u32(len(bits)) + b"".join(u32(value) for value in bits)
    return component(5, payload)


def value_tuple(*components: bytes) -> bytes:
    if len(components) > 0xFF:
        raise ValueError("value arity out of range")
    encoded = bytes((len(components),)) + b"".join(components)
    if len(encoded) > MAX_VALUE_TUPLE_BYTES:
        raise ValueError("canonical value tuple exceeds v1 cap")
    return encoded


def _validate_value_component(encoded: bytes, expected: int | tuple[int, ...]) -> None:
    if len(encoded) < 5:
        raise ValueError("truncated value component")
    tag = encoded[0]
    allowed = expected if isinstance(expected, tuple) else (expected,)
    if tag not in allowed:
        raise ValueError("wrong value component type")
    length = int.from_bytes(encoded[1:5], "big")
    payload = encoded[5:]
    if length != len(payload):
        raise ValueError("value component length mismatch")
    if tag == VALUE_NULL:
        if payload:
            raise ValueError("null payload must be empty")
    elif tag == VALUE_STRING:
        payload.decode("utf-8")
    elif tag == VALUE_BOOL:
        if payload not in (b"\x00", b"\x01"):
            raise ValueError("bool payload must be exactly zero or one")
    elif tag == VALUE_I64:
        if len(payload) != 8:
            raise ValueError("integer value must contain exactly eight payload bytes")
    elif tag == VALUE_F32:
        if len(payload) != 4:
            raise ValueError("f32 value must contain exactly four payload bytes")
    elif tag == VALUE_VEC_F32:
        if len(payload) < 4:
            raise ValueError("truncated vector count")
        count = int.from_bytes(payload[:4], "big")
        if not 1 <= count <= MAX_VECTOR_DIMENSION or len(payload) != 4 + count * 4:
            raise ValueError("invalid vector payload")
    else:
        raise ValueError("unknown value component type")


def _validate_value_tuple(relation: int, encoded: bytes) -> None:
    if not encoded or len(encoded) > MAX_VALUE_TUPLE_BYTES:
        raise ValueError("canonical value tuple length out of range")
    expected_values = RELATION_SIGNATURES[relation][1]
    if encoded[0] != len(expected_values):
        raise ValueError("wrong value arity")
    cursor = 1
    for expected in expected_values:
        if cursor + 5 > len(encoded):
            raise ValueError("truncated value component")
        length = int.from_bytes(encoded[cursor + 1 : cursor + 5], "big")
        end = cursor + 5 + length
        if end > len(encoded):
            raise ValueError("truncated value payload")
        _validate_value_component(encoded[cursor:end], expected)
        cursor = end
    if cursor != len(encoded):
        raise ValueError("trailing value tuple bytes")


def digest(data: bytes) -> bytes:
    return hashlib.sha256(data).digest()


def record_digest(position_bytes: bytes, values: bytes) -> bytes:
    relation = _validate_position(position_bytes)
    _validate_value_tuple(relation, values)
    return digest(RECORD_DOMAIN + u16(len(position_bytes)) + position_bytes + values)


@dataclass(frozen=True)
class Record:
    position: bytes
    digest: bytes

    def __post_init__(self) -> None:
        _validate_position(self.position)
        if len(self.digest) != hashlib.sha256().digest_size:
            raise ValueError("record digest must be exactly 32 bytes")


def update_records(
    hasher: "hashlib._Hash", records: Iterable[Record], expected_relation: int
) -> int:
    count = 0
    previous: bytes | None = None
    for record in records:
        if _validate_position(record.position) != expected_relation:
            raise ValueError("record belongs to the wrong relation segment")
        if previous is not None and record.position <= previous:
            raise ValueError("records are not in strict position order")
        hasher.update(b"\x02")
        hasher.update(u16(len(record.position)))
        hasher.update(record.position)
        hasher.update(record.digest)
        previous = record.position
        count += 1
    return count


def relation_digest(relation: int, records: Sequence[Record]) -> bytes:
    if relation not in RELATION_SIGNATURES:
        raise ValueError("relation ordinal out of range")
    if len(records) > MAX_RECORDS:
        raise ValueError("record count exceeds v1 cap")
    hasher = hashlib.sha256()
    hasher.update(RELATION_DOMAIN)
    hasher.update(u16(relation))
    count = update_records(hasher, records, relation)
    hasher.update(b"\xff")
    hasher.update(u64(count))
    return hasher.digest()


def store_digest(relations: Sequence[Sequence[Record]]) -> bytes:
    if len(relations) != MAX_RELATION_ORDINAL:
        raise ValueError("v1 requires exactly 16 base relations")
    if sum(len(records) for records in relations) > MAX_RECORDS:
        raise ValueError("record count exceeds v1 cap")
    hasher = hashlib.sha256()
    hasher.update(STORE_DOMAIN)
    hasher.update(u16(len(relations)))
    total = 0
    for ordinal, records in enumerate(relations, start=1):
        hasher.update(b"\x01")
        hasher.update(u16(ordinal))
        count = update_records(hasher, records, ordinal)
        hasher.update(b"\x03")
        hasher.update(u64(count))
        total += count
    hasher.update(b"\xff")
    hasher.update(u64(total))
    return hasher.digest()


def fixtures() -> dict[str, str]:
    empty_meta = position(1, key_string(""))
    meta_a = position(1, key_string("a"))
    meta_nul = position(1, key_string("a\0b"))
    meta_k = position(1, key_string("k"))
    meta_k_empty = record_digest(meta_k, value_tuple(value_string("")))

    nullable_position = position(9, key_string("a"), key_string("b"))
    nullable_null = record_digest(nullable_position, value_tuple(
        value_i64(1), value_i64(2), value_i64(3), value_null()
    ))
    nullable_empty = record_digest(nullable_position, value_tuple(
        value_i64(1), value_i64(2), value_i64(3), value_string("")
    ))

    vector_position = position(6, key_string("00000000000000000000000001"))
    vector = record_digest(vector_position, value_tuple(
        value_vec_f32_bits((0x00000000, 0x80000000, 0x7FC01234)),
        value_string("active"),
    ))

    tag_v2_position = position(
        4,
        key_string("tag"),
        key_string("active"),
        key_i64(-2_339_287_341_433_096_402),
        key_string("00000000000000000000000001"),
    )
    tag_v2 = record_digest(tag_v2_position, value_tuple())

    remote_position = position(
        13,
        key_string("00000000000000000000000001"),
        key_string("00000000000000000000000002"),
        key_string("00000000000000000000000003"),
    )
    remote_half = record_digest(
        remote_position, value_tuple(value_f32_bits(0x3F000000))
    )

    retry_order_position = position(
        15, key_string("epoch"), key_i64(1), key_string("key")
    )
    retry_order_true = record_digest(
        retry_order_position, value_tuple(value_bool(True))
    )

    meta_x = Record(meta_a, record_digest(meta_a, value_tuple(value_string("x"))))
    meta_b = position(1, key_string("b"))
    meta_y = Record(meta_b, record_digest(meta_b, value_tuple(value_string("y"))))
    relations: list[Sequence[Record]] = [[meta_x, meta_y]] + [[] for _ in range(15)]
    empty_relations: list[Sequence[Record]] = [[] for _ in range(16)]

    return {
        "position_meta_empty": empty_meta.hex(),
        "position_meta_a": meta_a.hex(),
        "position_meta_a_nul_b": meta_nul.hex(),
        "key_i64_minus_one": key_i64(-1).hex(),
        "key_i64_zero": key_i64(0).hex(),
        "key_i64_one": key_i64(1).hex(),
        "record_meta_k_empty": meta_k_empty.hex(),
        "record_nullable_null": nullable_null.hex(),
        "record_nullable_empty": nullable_empty.hex(),
        "record_vector_bits": vector.hex(),
        "position_node_tag_v2": tag_v2_position.hex(),
        "record_node_tag_v2": tag_v2.hex(),
        "record_remote_edge_half": remote_half.hex(),
        "record_feedback_retry_order_true": retry_order_true.hex(),
        "relation_meta_empty": relation_digest(1, ()).hex(),
        "store_empty": store_digest(empty_relations).hex(),
        "relation_meta_two_records": relation_digest(1, (meta_x, meta_y)).hex(),
        "store_two_meta_records": store_digest(relations).hex(),
    }


def main() -> None:
    print(json.dumps(fixtures(), sort_keys=True, indent=2))


if __name__ == "__main__":
    main()

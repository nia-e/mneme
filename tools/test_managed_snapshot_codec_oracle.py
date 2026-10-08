#!/usr/bin/env python3

import unittest

if __package__:
    from tools import managed_snapshot_codec_oracle as oracle
else:
    import managed_snapshot_codec_oracle as oracle


EXPECTED = {
    "key_i64_minus_one": "027fffffffffffffff",
    "key_i64_one": "028000000000000001",
    "key_i64_zero": "028000000000000000",
    "position_meta_a": "00010101610000",
    "position_meta_a_nul_b": "000101016100ff620000",
    "position_meta_empty": "000101010000",
    "position_node_tag_v2": "000404017461670000016163746976650000025f892e8e9a4ff72e0130303030303030303030303030303030303030303030303030310000",
    "record_feedback_retry_order_true": "03b68a432fc40cdf5a77afd475255e9e106672172ffed6203a596278a1335ed2",
    "record_meta_k_empty": "b19e83dd00e2b391f8ed1e6ceaa867a67f136aceec26f5ebb3da612715dfbfe4",
    "record_node_tag_v2": "390a03abd5c01dd8ca943b3d6d0978af432e95277bbf4dcd81727df16475e410",
    "record_nullable_empty": "a9b451c7e213be524c965eeb12fb95f280fe2d6623d5598e617c91e55be0dd04",
    "record_nullable_null": "fd9088c28c88b5d06c574c16cd892d1b78b5396f48d621bb4904a795e6ded6a3",
    "record_remote_edge_half": "b7ccaaf8f29e81ff07e22bbcfe8cbed81969474221085f22269ab2c850c97182",
    "record_vector_bits": "fcf60fb3acc1aac20c95b8e6e4f26627e5519bfa489811908678a99446ec4628",
    "relation_meta_empty": "fab60c7450e52e681ccbc3cd4ae9dc0d936c60459f3905b44231d21a289d25c8",
    "relation_meta_two_records": "2b73983c6d9f9dfd17335e7d51b2dab3e5c8b487d61beb008c3d9ed5c6d26d4a",
    "store_empty": "86acf9aa567df6e35951f7b29fccc829afca699ad854fd9800b03bcec82ce49e",
    "store_two_meta_records": "615f115cab07f05debcd39baaae0ad3d22c244eb2476845b7f1f721058f1ff6f",
}


class ManagedSnapshotCodecOracleTests(unittest.TestCase):
    def test_known_answers_are_pinned(self) -> None:
        self.assertEqual(oracle.fixtures(), EXPECTED)

    def test_nullable_and_empty_string_are_distinct(self) -> None:
        actual = oracle.fixtures()
        self.assertNotEqual(
            actual["record_nullable_null"], actual["record_nullable_empty"]
        )

    def test_string_and_integer_key_encodings_preserve_order(self) -> None:
        strings = ["", "\0", "\0a", "a", "a\0", "aa", "b"]
        encoded_strings = [oracle.key_string(value) for value in strings]
        self.assertEqual(encoded_strings, sorted(encoded_strings))

        integers = [-(1 << 63), -2, -1, 0, 1, 2, (1 << 63) - 1]
        encoded_integers = [oracle.key_i64(value) for value in integers]
        self.assertEqual(encoded_integers, sorted(encoded_integers))

    def test_store_rejects_wrong_relation_count_and_record_order(self) -> None:
        with self.assertRaises(ValueError):
            oracle.store_digest([[] for _ in range(15)])

        first = oracle.position(1, oracle.key_string("a"))
        second = oracle.position(1, oracle.key_string("b"))
        records = [
            oracle.Record(second, bytes(32)),
            oracle.Record(first, bytes(32)),
        ]
        with self.assertRaises(ValueError):
            oracle.relation_digest(1, records)

    def test_relation_vocabulary_and_segment_membership_are_enforced(self) -> None:
        with self.assertRaises(ValueError):
            oracle.position(17, oracle.key_string("x"))
        with self.assertRaises(ValueError):
            oracle.position(4, oracle.key_string("too-few"))

        node_position = oracle.position(2, oracle.key_string("node"))
        node = oracle.Record(node_position, bytes(32))
        with self.assertRaises(ValueError):
            oracle.relation_digest(1, [node])

    def test_zero_value_and_component_shapes_are_not_conflated(self) -> None:
        tag_v2 = oracle.position(
            4,
            oracle.key_string("tag"),
            oracle.key_string("active"),
            oracle.key_i64(-2_339_287_341_433_096_402),
            oracle.key_string("00000000000000000000000001"),
        )
        self.assertEqual(
            oracle.record_digest(tag_v2, oracle.value_tuple()).hex(),
            EXPECTED["record_node_tag_v2"],
        )
        with self.assertRaises(ValueError):
            oracle.record_digest(tag_v2, oracle.value_tuple(oracle.value_string("")))

        malformed_bool = bytes((oracle.VALUE_BOOL,)) + oracle.u32(1) + b"\x02"
        retry_order = oracle.position(
            15,
            oracle.key_string("epoch"),
            oracle.key_i64(1),
            oracle.key_string("key"),
        )
        with self.assertRaises(ValueError):
            oracle.record_digest(retry_order, oracle.value_tuple(malformed_bool))

        with self.assertRaises(ValueError):
            oracle.value_bool(1)
        with self.assertRaises(ValueError):
            oracle.component(oracle.VALUE_NULL, b"not-null")

    def test_malformed_key_and_value_encodings_are_rejected(self) -> None:
        for malformed in (
            b"\x01unterminated",
            b"\x01bad\x00\x01",
            b"\x01\xff\x00\x00",
            b"\x01ok\x00\x00trailing",
        ):
            with self.assertRaises((UnicodeDecodeError, ValueError)):
                oracle.position(1, malformed)

        meta = oracle.position(1, oracle.key_string("k"))
        valid = oracle.value_tuple(oracle.value_string("v"))
        with self.assertRaises(ValueError):
            oracle.record_digest(meta, valid + b"trailing")

    def test_canonical_value_and_position_caps_are_exact(self) -> None:
        exact_payload = "x" * (oracle.MAX_VALUE_TUPLE_BYTES - 6)
        self.assertEqual(
            len(oracle.value_tuple(oracle.value_string(exact_payload))),
            oracle.MAX_VALUE_TUPLE_BYTES,
        )
        with self.assertRaises(ValueError):
            oracle.value_tuple(oracle.value_string(exact_payload + "x"))

        exact_key = oracle.position(1, oracle.key_string("\0" * 1_186))
        self.assertEqual(len(exact_key), oracle.MAX_POSITION_BYTES)
        with self.assertRaises(ValueError):
            oracle.position(1, oracle.key_string("\0" * 1_187))

    def test_every_frozen_relation_signature_has_a_codec_path(self) -> None:
        for relation, (key_types, value_types) in oracle.RELATION_SIGNATURES.items():
            keys = tuple(
                oracle.key_string("x")
                if key_type == oracle.KEY_STRING
                else oracle.key_i64(0)
                for key_type in key_types
            )
            values = []
            for value_type in value_types:
                tag = value_type[0] if isinstance(value_type, tuple) else value_type
                values.append(
                    {
                        oracle.VALUE_NULL: oracle.value_null,
                        oracle.VALUE_STRING: lambda: oracle.value_string("x"),
                        oracle.VALUE_BOOL: lambda: oracle.value_bool(True),
                        oracle.VALUE_I64: lambda: oracle.value_i64(0),
                        oracle.VALUE_F32: lambda: oracle.value_f32_bits(0x3F000000),
                        oracle.VALUE_VEC_F32: lambda: oracle.value_vec_f32_bits(
                            (0x3F800000,)
                        ),
                    }[tag]()
                )
            encoded_position = oracle.position(relation, *keys)
            self.assertEqual(
                len(oracle.record_digest(encoded_position, oracle.value_tuple(*values))),
                32,
            )


if __name__ == "__main__":
    unittest.main()

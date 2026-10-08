#!/usr/bin/env python3

import unittest
from pathlib import Path

if __package__:
    from tools import managed_snapshot_framing_oracle as oracle
else:
    import managed_snapshot_framing_oracle as oracle


EXPECTED = {
    "bodies_preamble": "4d4e454d454d4e4600010200e748d8011c4fe31372ad1f34d1717803547d3b51984aa8aa9ca9b2eec4c48909",
    "body_content_absent": "d3c7a55a8c24b5f709b4790d35f874ad2159f9fc42b69022e6934a5f937e9e5c",
    "body_content_present_x_empty": "3621b7bce6b338a76c21632a5fd39387841c0039315fda41d4cabb00d4a2b67a",
    "body_external_map_x": "9b5f11bfbe0dd9edb476224f6d7a99ba73205dbb4f4e500c6c5be398323aab14",
    "body_local_map_x": "ec28650d649c555d297c2712ee9811a2348e1d854e8cb9303879b35af74e101e",
    "census_frame_digest": "9e6b329ad2bd5b789d4f7f6d92bde018f8f0f41d990a6f88c349d8bde1ab9a18",
    "census_payload_sha256": "c3990ad396cffe0f803b2489f8e89d9dee97789fa201bf0eacdda1070c374252",
    "empty_records_bytes": 100,
    "empty_records_sha256": "d2b364c708a56973347389156eb92aef2b2fe5c687603e0de529c851a78970f3",
    "external_frame_digest": "635a3b3475e9123d0a13aaacd1cc92c5ed29b79c4f412beaf07bc3a2af2ec262",
    "external_payload_sha256": "99a537b0ee6dd4df0497732b0e41a0640854495abbcec990e806b156db98a654",
    "local_frame_digest": "f7bb6c7dd8eb73d35d160d85ee27b61804cd5cc83b8d8469690d0d1cffcd62b5",
    "local_payload_sha256": "6775f1c728d8687c6a77559e5e675744a2b4a926b994156f09286f0bdbd031f9",
    "opaque_record_frame_digest": "75ed66908522b12c2cbd943cfacedb5b847f6bed5fa8e39846f3636e9ecb4210",
    "opaque_record_inventory_digest": "c6d66d7f0a4f0b59cff02533bc7ba51b3ac3076a3b4b883892986ee1366befe0",
    "opaque_record_stream_bytes": 157,
    "opaque_record_stream_sha256": "123448d2b70e063393bbea82e3007efe0632291e4f203fbef9a73b6546515bea",
    "policy_fingerprint": "e748d8011c4fe31372ad1f34d1717803547d3b51984aa8aa9ca9b2eec4c48909",
    "policy_transcript_bytes": 2656,
    "records_preamble": "4d4e454d454d4e4600010100e748d8011c4fe31372ad1f34d1717803547d3b51984aa8aa9ca9b2eec4c48909",
}


class ManagedSnapshotFramingOracleTests(unittest.TestCase):
    def test_known_answers_are_pinned(self) -> None:
        self.assertEqual(oracle.fixtures(), EXPECTED)

    def test_retained_policy_transcript_is_the_hashed_transcript(self) -> None:
        transcript = (
            Path(__file__).resolve().parent
            / "fixtures/managed_snapshot_framing_transcript.txt"
        ).read_bytes()
        self.assertEqual(transcript, oracle.policy_transcript())

    def test_footer_boundary_is_fixed_even_for_an_empty_stream(self) -> None:
        encoded = oracle.stream(oracle.RECORDS, ())
        self.assertEqual(len(encoded), oracle.PREAMBLE_BYTES + oracle.FOOTER_BYTES)
        self.assertEqual(
            encoded[-oracle.FOOTER_BYTES : -oracle.FOOTER_BYTES + 8],
            oracle.FOOTER_MAGIC,
        )

    def test_stream_order_and_frame_domains_are_enforced(self) -> None:
        first = oracle.Frame(oracle.RECORDS, 1, 2, 0, 1, b"{}")
        lower_relation = oracle.Frame(oracle.RECORDS, 1, 1, 0, 1, b"{}")
        with self.assertRaises(ValueError):
            oracle.stream(oracle.RECORDS, (first, lower_relation))

        skipped = oracle.Frame(oracle.BODIES, 2, 0, 1, 1, b"{}")
        with self.assertRaises(ValueError):
            oracle.stream(oracle.BODIES, (skipped,))

        with self.assertRaises(ValueError):
            oracle.Frame(oracle.BODIES, 1, 0, 0, 1, b"{}")

        short_record = oracle.Frame(oracle.RECORDS, 1, 1, 0, 1, b"{}")
        next_record = oracle.Frame(oracle.RECORDS, 1, 1, 1, 1, b"{}")
        with self.assertRaises(ValueError):
            oracle.stream(oracle.RECORDS, (short_record, next_record))

        short_body = oracle.Frame(oracle.BODIES, 2, 0, 0, 1, b"{}")
        next_body = oracle.Frame(oracle.BODIES, 2, 0, 1, 1, b"{}")
        with self.assertRaises(ValueError):
            oracle.stream(oracle.BODIES, (short_body, next_body))

    def test_payload_and_item_caps_fail_before_encoding(self) -> None:
        with self.assertRaises(ValueError):
            oracle.Frame(oracle.RECORDS, 1, 1, 0, 0, b"{}")
        with self.assertRaises(ValueError):
            oracle.Frame(
                oracle.RECORDS,
                1,
                1,
                0,
                1,
                b"x" * (oracle.MAX_RECORD_PAYLOAD_BYTES + 1),
            )

    def test_body_logical_digests_reject_ambiguous_inventory(self) -> None:
        empty_sha = oracle.sha256(b"")
        with self.assertRaises(ValueError):
            oracle.body_content_digest(False, ((b"x", 0, empty_sha),))
        with self.assertRaises(ValueError):
            oracle.body_content_digest(
                True,
                ((b"x", 0, empty_sha), (b"x", 0, empty_sha)),
            )
        with self.assertRaises(ValueError):
            oracle.body_map_digest(
                oracle.BODY_LOCAL_MAP_DOMAIN,
                ((b"fs://x", b"x", 0),),
            )
        with self.assertRaises(ValueError):
            oracle.body_map_digest(
                oracle.BODY_EXTERNAL_MAP_DOMAIN,
                ((b"inline://x", b"x", 1),),
            )


if __name__ == "__main__":
    unittest.main()

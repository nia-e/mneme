/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
*/

//! Bounded validation and exact decoding for MessagePack stored by the engine.
//!
//! Serde is intentionally not the first parser here. Some serde data models
//! allocate from lengths declared by the input, so an untrusted stored blob
//! must pass this allocation-free envelope scan before it reaches a decoder.

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::io::{self, Write};

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

const SCANNER_STACK_CAPACITY: usize = 64;

/// Stable identity for the allocation-free exact-envelope scanner, typed
/// rmp-serde decode, and the two exact-byte canonical serializer modes.
pub(crate) const STORED_MSGPACK_EXACT_CODEC_POLICY_FINGERPRINT_V1: [u8; 32] = [
    223, 26, 165, 163, 160, 135, 240, 199, 21, 59, 46, 28, 103, 47, 5, 16, 1, 173, 100, 223, 141,
    155, 168, 110, 249, 2, 10, 224, 144, 223, 106, 115,
];
/// Stable identity for the exact row-value envelope profile.
pub(crate) const STORED_MSGPACK_ROW_POLICY_FINGERPRINT_V1: [u8; 32] = [
    236, 2, 111, 211, 226, 82, 229, 119, 196, 207, 79, 35, 30, 131, 102, 28, 184, 79, 223, 230, 30,
    10, 129, 200, 84, 7, 171, 24, 182, 30, 32, 235,
];
/// Stable identity for the exact relation-catalog envelope profile.
pub(crate) const STORED_MSGPACK_RELATION_CATALOG_POLICY_FINGERPRINT_V1: [u8; 32] = [
    85, 156, 20, 219, 180, 85, 18, 133, 235, 186, 202, 182, 221, 62, 91, 121, 79, 130, 1, 78, 129,
    63, 61, 226, 35, 225, 206, 249, 244, 97, 221, 234,
];

#[cfg(test)]
const STORED_MSGPACK_EXACT_CODEC_POLICY_FINGERPRINT_DOMAIN_V1: &[u8] =
    b"mnestic.stored-msgpack-exact-codec.policy-fingerprint-transcript.v1\0";
#[cfg(test)]
const STORED_MSGPACK_PROFILE_POLICY_FINGERPRINT_DOMAIN_V1: &[u8] =
    b"mnestic.stored-msgpack-profile.policy-fingerprint-transcript.v1\0";
#[cfg(test)]
const STORED_MSGPACK_EXACT_CODEC_POLICY_DESCRIPTION_V1: &str = concat!(
    "mnestic.stored-msgpack-exact-codec-policy.v1\n",
    "scanner=allocation-free iterative full marker grammar, reserved-marker rejection, exact EOF, checked lengths\n",
    "decode=scan first, then typed rmp-serde from the same bytes with scanner-depth-plus-two, then require decoder EOF\n",
    "canonical=exact comparator supports rmp-serde compact and struct-map modes; decode_exact itself does not require canonical reserialization\n",
    "diagnostics=stable bounded kind,offset,limit,observed only; attacker payload bytes omitted\n",
    "stack-capacity=64\n",
);

/// The two stored MessagePack shapes with deliberately conservative budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoredMsgpackProfile {
    /// A tuple value blob. Its root is always an array.
    Row,
    /// A relation-catalog record. Legacy records are arrays; current records
    /// are self-describing maps.
    RelationCatalog,
}

#[derive(Debug, Clone, Copy)]
struct Limits {
    bytes: usize,
    depth: usize,
    tokens: u64,
    containers: u64,
    items_per_container: u64,
    root: RootRequirement,
}

impl StoredMsgpackProfile {
    pub(crate) const fn byte_limit(self) -> usize {
        self.limits().bytes
    }

    const fn limits(self) -> Limits {
        match self {
            Self::Row => Limits {
                bytes: 1024 * 1024,
                depth: 32,
                tokens: 65_536,
                containers: 16_384,
                items_per_container: 16_384,
                root: RootRequirement::Array,
            },
            Self::RelationCatalog => Limits {
                bytes: 4 * 1024 * 1024,
                depth: 64,
                tokens: 262_144,
                containers: 65_536,
                items_per_container: 65_536,
                root: RootRequirement::ArrayOrMap,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootRequirement {
    Array,
    ArrayOrMap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootKind {
    Scalar,
    Array,
    Map,
}

/// The accepted root shape of one allocation-free validated envelope.
///
/// Stored profiles currently admit only container roots. Keeping this enum
/// narrower than MessagePack's full grammar prevents a caller from treating a
/// rejected scalar as protocol evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoredMsgpackRoot {
    Array,
    Map,
}

/// Opaque evidence that one exact byte slice passed a stored profile's scan.
///
/// The private fields bind the evidence to both the bytes and the profile. A
/// typed decoder can therefore reuse the completed scan without exposing a
/// crate-wide "trust me, these bytes were scanned" entry point.
pub(crate) struct ScannedMsgpack<'a> {
    encoded: &'a [u8],
    profile: StoredMsgpackProfile,
    root: StoredMsgpackRoot,
}

impl<'a> ScannedMsgpack<'a> {
    pub(crate) const fn root(&self) -> StoredMsgpackRoot {
        self.root
    }

    pub(crate) fn decode<T>(&self) -> Result<T, StoredMsgpackError>
    where
        T: Deserialize<'a>,
    {
        decode_scanned_exact(self.encoded, self.profile)
    }
}

/// Stable error categories for corrupt or non-canonical stored MessagePack.
///
/// The associated [`StoredMsgpackError`] contains only bounded counters and
/// offsets. It never copies, formats, or retains bytes supplied by the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoredMsgpackErrorKind {
    Empty,
    ByteLimit,
    ReservedMarker,
    Truncated,
    TrailingBytes,
    WrongRoot,
    DepthLimit,
    TokenLimit,
    ContainerLimit,
    ItemLimit,
    LengthOverflow,
    DecodeFailed,
    DecodeDidNotConsumeEnvelope,
    EncodeFailed,
    CanonicalMismatch,
    CanonicalLengthMismatch,
}

/// A bounded, input-independent diagnostic for stored MessagePack failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StoredMsgpackError {
    kind: StoredMsgpackErrorKind,
    offset: usize,
    limit: u64,
    observed: u64,
}

impl StoredMsgpackError {
    const fn new(kind: StoredMsgpackErrorKind, offset: usize, limit: u64, observed: u64) -> Self {
        Self {
            kind,
            offset,
            limit,
            observed,
        }
    }

    pub(crate) const fn kind(self) -> StoredMsgpackErrorKind {
        self.kind
    }

    pub(crate) const fn offset(self) -> usize {
        self.offset
    }

    pub(crate) const fn limit(self) -> u64 {
        self.limit
    }

    pub(crate) const fn observed(self) -> u64 {
        self.observed
    }
}

impl Display for StoredMsgpackError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "stored MessagePack {:?} at offset {} (limit {}, observed {})",
            self.kind, self.offset, self.limit, self.observed
        )
    }
}

impl Error for StoredMsgpackError {}

/// Validate one complete MessagePack object without allocation or recursion.
///
/// This checks the complete marker grammar, exact EOF, the profile's root
/// shape, and caps on bytes, depth, total tokens, total containers, and child
/// slots declared by any one container. String payloads remain bytes at this
/// layer; UTF-8 is a semantic concern for the typed serde decoder.
pub(crate) fn validate_exact_envelope(
    encoded: &[u8],
    profile: StoredMsgpackProfile,
) -> Result<(), StoredMsgpackError> {
    scan_exact_envelope(encoded, profile).map(drop)
}

/// Validate one complete MessagePack object and return byte-bound evidence.
///
/// This performs the same allocation-free, iterative scan as
/// [`validate_exact_envelope`]. The returned token exposes only the admitted
/// root kind and a typed decode method that cannot be detached from the
/// validated slice or profile.
pub(crate) fn scan_exact_envelope<'a>(
    encoded: &'a [u8],
    profile: StoredMsgpackProfile,
) -> Result<ScannedMsgpack<'a>, StoredMsgpackError> {
    let limits = profile.limits();
    if encoded.is_empty() {
        return Err(StoredMsgpackError::new(
            StoredMsgpackErrorKind::Empty,
            0,
            1,
            0,
        ));
    }
    if encoded.len() > limits.bytes {
        return Err(StoredMsgpackError::new(
            StoredMsgpackErrorKind::ByteLimit,
            0,
            limits.bytes as u64,
            usize_to_u64_saturating(encoded.len()),
        ));
    }

    let mut position = 0usize;
    let mut stack = [0u64; SCANNER_STACK_CAPACITY];
    let mut stack_len = 0usize;
    let mut tokens = 0u64;
    let mut containers = 0u64;
    let mut is_root = true;
    let mut accepted_root = None;

    loop {
        if stack_len > 0 {
            let remaining = &mut stack[stack_len - 1];
            debug_assert!(*remaining > 0);
            *remaining -= 1;
        }

        let marker_offset = position;
        let marker = take_u8(encoded, &mut position, marker_offset)?;
        tokens = tokens.checked_add(1).ok_or_else(|| {
            StoredMsgpackError::new(
                StoredMsgpackErrorKind::LengthOverflow,
                marker_offset,
                limits.tokens,
                u64::MAX,
            )
        })?;
        if tokens > limits.tokens {
            return Err(StoredMsgpackError::new(
                StoredMsgpackErrorKind::TokenLimit,
                marker_offset,
                limits.tokens,
                tokens,
            ));
        }

        let token = parse_marker(encoded, &mut position, marker, marker_offset)?;
        if is_root {
            let root = match (limits.root, token.root_kind) {
                (RootRequirement::Array, RootKind::Array)
                | (RootRequirement::ArrayOrMap, RootKind::Array) => StoredMsgpackRoot::Array,
                (RootRequirement::ArrayOrMap, RootKind::Map) => StoredMsgpackRoot::Map,
                _ => {
                    return Err(StoredMsgpackError::new(
                        StoredMsgpackErrorKind::WrongRoot,
                        marker_offset,
                        root_requirement_code(limits.root),
                        root_kind_code(token.root_kind),
                    ));
                }
            };
            accepted_root = Some(root);
            is_root = false;
        }

        if let Some(child_slots) = token.child_slots {
            containers = containers.checked_add(1).ok_or_else(|| {
                StoredMsgpackError::new(
                    StoredMsgpackErrorKind::LengthOverflow,
                    marker_offset,
                    limits.containers,
                    u64::MAX,
                )
            })?;
            if containers > limits.containers {
                return Err(StoredMsgpackError::new(
                    StoredMsgpackErrorKind::ContainerLimit,
                    marker_offset,
                    limits.containers,
                    containers,
                ));
            }
            if child_slots > limits.items_per_container {
                return Err(StoredMsgpackError::new(
                    StoredMsgpackErrorKind::ItemLimit,
                    marker_offset,
                    limits.items_per_container,
                    child_slots,
                ));
            }

            let depth = stack_len.checked_add(1).ok_or_else(|| {
                StoredMsgpackError::new(
                    StoredMsgpackErrorKind::LengthOverflow,
                    marker_offset,
                    limits.depth as u64,
                    u64::MAX,
                )
            })?;
            if depth > limits.depth || depth > SCANNER_STACK_CAPACITY {
                return Err(StoredMsgpackError::new(
                    StoredMsgpackErrorKind::DepthLimit,
                    marker_offset,
                    limits.depth as u64,
                    usize_to_u64_saturating(depth),
                ));
            }
            if child_slots != 0 {
                stack[stack_len] = child_slots;
                stack_len += 1;
            }
        }

        while stack_len > 0 && stack[stack_len - 1] == 0 {
            stack_len -= 1;
        }

        if stack_len == 0 {
            if position == encoded.len() {
                let root = accepted_root.ok_or_else(|| {
                    StoredMsgpackError::new(
                        StoredMsgpackErrorKind::WrongRoot,
                        0,
                        root_requirement_code(limits.root),
                        root_kind_code(RootKind::Scalar),
                    )
                })?;
                return Ok(ScannedMsgpack {
                    encoded,
                    profile,
                    root,
                });
            }
            return Err(StoredMsgpackError::new(
                StoredMsgpackErrorKind::TrailingBytes,
                position,
                0,
                usize_to_u64_saturating(encoded.len() - position),
            ));
        }
    }
}

/// Decode one exact, pre-budgeted MessagePack object.
pub(crate) fn decode_exact<'de, T>(
    encoded: &'de [u8],
    profile: StoredMsgpackProfile,
) -> Result<T, StoredMsgpackError>
where
    T: Deserialize<'de>,
{
    scan_exact_envelope(encoded, profile)?.decode()
}

fn decode_scanned_exact<'de, T>(
    encoded: &'de [u8],
    profile: StoredMsgpackProfile,
) -> Result<T, StoredMsgpackError>
where
    T: Deserialize<'de>,
{
    let limits = profile.limits();
    let mut decoder = rmp_serde::Deserializer::from_read_ref(encoded);
    // rmp-serde rejects when its counter reaches zero, so one extra counter
    // unit aligns its accepted nesting with the scanner's inclusive limit.
    // Extensions are scalars in the MessagePack grammar but rmp-serde wraps
    // one in a synthetic newtype sequence, consuming one additional unit.
    decoder.set_max_depth(limits.depth.saturating_add(2));
    let value = T::deserialize(&mut decoder)
        .map_err(|_| StoredMsgpackError::new(StoredMsgpackErrorKind::DecodeFailed, 0, 0, 0))?;
    match IgnoredAny::deserialize(&mut decoder) {
        Err(rmp_serde::decode::Error::InvalidMarkerRead(error))
            if error.kind() == io::ErrorKind::UnexpectedEof => {}
        Ok(_) => {
            return Err(StoredMsgpackError::new(
                StoredMsgpackErrorKind::DecodeDidNotConsumeEnvelope,
                0,
                0,
                1,
            ));
        }
        Err(_) => {
            return Err(StoredMsgpackError::new(
                StoredMsgpackErrorKind::DecodeFailed,
                0,
                0,
                0,
            ));
        }
    }
    Ok(value)
}

/// The writer configuration used when proving an existing encoding canonical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CanonicalMode {
    /// rmp-serde's default compact representation (structs as tuples).
    Compact,
    /// rmp-serde's self-describing representation (structs as maps).
    StructMap,
}

/// Compare a serializer's output to an existing byte slice without allocating.
#[derive(Debug)]
pub(crate) struct ExactBytesWriter<'a> {
    expected: &'a [u8],
    position: usize,
    mismatch_at: Option<usize>,
    overflowed: bool,
}

impl<'a> ExactBytesWriter<'a> {
    pub(crate) const fn new(expected: &'a [u8]) -> Self {
        Self {
            expected,
            position: 0,
            mismatch_at: None,
            overflowed: false,
        }
    }

    pub(crate) const fn bytes_written(&self) -> usize {
        self.position
    }

    pub(crate) fn finish(self) -> Result<(), StoredMsgpackError> {
        if self.overflowed {
            return Err(StoredMsgpackError::new(
                StoredMsgpackErrorKind::LengthOverflow,
                self.position,
                usize_to_u64_saturating(self.expected.len()),
                u64::MAX,
            ));
        }
        if let Some(offset) = self.mismatch_at {
            return Err(StoredMsgpackError::new(
                StoredMsgpackErrorKind::CanonicalMismatch,
                offset,
                usize_to_u64_saturating(self.expected.len()),
                usize_to_u64_saturating(self.position),
            ));
        }
        if self.position != self.expected.len() {
            return Err(StoredMsgpackError::new(
                StoredMsgpackErrorKind::CanonicalLengthMismatch,
                self.position.min(self.expected.len()),
                usize_to_u64_saturating(self.expected.len()),
                usize_to_u64_saturating(self.position),
            ));
        }
        Ok(())
    }
}

impl Write for ExactBytesWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let start = self.position;
        let Some(end) = start.checked_add(bytes.len()) else {
            self.overflowed = true;
            self.mismatch_at.get_or_insert(start);
            self.position = usize::MAX;
            return Ok(bytes.len());
        };

        if self.mismatch_at.is_none() {
            let available = self.expected.len().saturating_sub(start);
            let compared = available.min(bytes.len());
            if compared != 0 {
                if let Some(relative) = bytes[..compared]
                    .iter()
                    .zip(&self.expected[start..start + compared])
                    .position(|(actual, expected)| actual != expected)
                {
                    self.mismatch_at = Some(start + relative);
                }
            }
            if self.mismatch_at.is_none() && bytes.len() > available {
                self.mismatch_at = Some(start + available);
            }
        }
        self.position = end;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Re-serialize a value directly into an exact comparator and require byte-for-
/// byte equality with `encoded`.
pub(crate) fn require_canonical<T>(
    value: &T,
    encoded: &[u8],
    mode: CanonicalMode,
) -> Result<(), StoredMsgpackError>
where
    T: Serialize + ?Sized,
{
    let mut writer = ExactBytesWriter::new(encoded);
    let encoded_ok = match mode {
        CanonicalMode::Compact => value.serialize(&mut rmp_serde::Serializer::new(&mut writer)),
        CanonicalMode::StructMap => {
            value.serialize(&mut rmp_serde::Serializer::new(&mut writer).with_struct_map())
        }
    };
    encoded_ok
        .map_err(|_| StoredMsgpackError::new(StoredMsgpackErrorKind::EncodeFailed, 0, 0, 0))?;
    writer.finish()
}

#[derive(Debug, Clone, Copy)]
struct ParsedToken {
    root_kind: RootKind,
    child_slots: Option<u64>,
}

impl ParsedToken {
    const SCALAR: Self = Self {
        root_kind: RootKind::Scalar,
        child_slots: None,
    };

    const fn array(items: u64) -> Self {
        Self {
            root_kind: RootKind::Array,
            child_slots: Some(items),
        }
    }

    fn map(entries: u64, offset: usize) -> Result<Self, StoredMsgpackError> {
        let child_slots = entries.checked_mul(2).ok_or_else(|| {
            StoredMsgpackError::new(
                StoredMsgpackErrorKind::LengthOverflow,
                offset,
                u64::MAX,
                entries,
            )
        })?;
        Ok(Self {
            root_kind: RootKind::Map,
            child_slots: Some(child_slots),
        })
    }
}

fn parse_marker(
    encoded: &[u8],
    position: &mut usize,
    marker: u8,
    marker_offset: usize,
) -> Result<ParsedToken, StoredMsgpackError> {
    match marker {
        0x00..=0x7f | 0xc0 | 0xc2 | 0xc3 | 0xe0..=0xff => Ok(ParsedToken::SCALAR),
        0x80..=0x8f => ParsedToken::map(u64::from(marker & 0x0f), marker_offset),
        0x90..=0x9f => Ok(ParsedToken::array(u64::from(marker & 0x0f))),
        0xa0..=0xbf => {
            take_payload(encoded, position, usize::from(marker & 0x1f), marker_offset)?;
            Ok(ParsedToken::SCALAR)
        }
        0xc1 => Err(StoredMsgpackError::new(
            StoredMsgpackErrorKind::ReservedMarker,
            marker_offset,
            0,
            0,
        )),
        0xc4 => {
            let length = usize::from(take_u8(encoded, position, marker_offset)?);
            take_payload(encoded, position, length, marker_offset)?;
            Ok(ParsedToken::SCALAR)
        }
        0xc5 => {
            let length = usize::from(take_u16(encoded, position, marker_offset)?);
            take_payload(encoded, position, length, marker_offset)?;
            Ok(ParsedToken::SCALAR)
        }
        0xc6 => {
            let length =
                usize_from_u32(take_u32(encoded, position, marker_offset)?, marker_offset)?;
            take_payload(encoded, position, length, marker_offset)?;
            Ok(ParsedToken::SCALAR)
        }
        0xc7 => {
            let length = usize::from(take_u8(encoded, position, marker_offset)?);
            take_ext_payload(encoded, position, length, marker_offset)?;
            Ok(ParsedToken::SCALAR)
        }
        0xc8 => {
            let length = usize::from(take_u16(encoded, position, marker_offset)?);
            take_ext_payload(encoded, position, length, marker_offset)?;
            Ok(ParsedToken::SCALAR)
        }
        0xc9 => {
            let length =
                usize_from_u32(take_u32(encoded, position, marker_offset)?, marker_offset)?;
            take_ext_payload(encoded, position, length, marker_offset)?;
            Ok(ParsedToken::SCALAR)
        }
        0xca => fixed_scalar(encoded, position, 4, marker_offset),
        0xcb => fixed_scalar(encoded, position, 8, marker_offset),
        0xcc | 0xd0 => fixed_scalar(encoded, position, 1, marker_offset),
        0xcd | 0xd1 => fixed_scalar(encoded, position, 2, marker_offset),
        0xce | 0xd2 => fixed_scalar(encoded, position, 4, marker_offset),
        0xcf | 0xd3 => fixed_scalar(encoded, position, 8, marker_offset),
        0xd4 => fixed_scalar(encoded, position, 2, marker_offset),
        0xd5 => fixed_scalar(encoded, position, 3, marker_offset),
        0xd6 => fixed_scalar(encoded, position, 5, marker_offset),
        0xd7 => fixed_scalar(encoded, position, 9, marker_offset),
        0xd8 => fixed_scalar(encoded, position, 17, marker_offset),
        0xd9 => {
            let length = usize::from(take_u8(encoded, position, marker_offset)?);
            take_payload(encoded, position, length, marker_offset)?;
            Ok(ParsedToken::SCALAR)
        }
        0xda => {
            let length = usize::from(take_u16(encoded, position, marker_offset)?);
            take_payload(encoded, position, length, marker_offset)?;
            Ok(ParsedToken::SCALAR)
        }
        0xdb => {
            let length =
                usize_from_u32(take_u32(encoded, position, marker_offset)?, marker_offset)?;
            take_payload(encoded, position, length, marker_offset)?;
            Ok(ParsedToken::SCALAR)
        }
        0xdc => Ok(ParsedToken::array(u64::from(take_u16(
            encoded,
            position,
            marker_offset,
        )?))),
        0xdd => Ok(ParsedToken::array(u64::from(take_u32(
            encoded,
            position,
            marker_offset,
        )?))),
        0xde => ParsedToken::map(
            u64::from(take_u16(encoded, position, marker_offset)?),
            marker_offset,
        ),
        0xdf => ParsedToken::map(
            u64::from(take_u32(encoded, position, marker_offset)?),
            marker_offset,
        ),
    }
}

fn fixed_scalar(
    encoded: &[u8],
    position: &mut usize,
    length: usize,
    marker_offset: usize,
) -> Result<ParsedToken, StoredMsgpackError> {
    take_payload(encoded, position, length, marker_offset)?;
    Ok(ParsedToken::SCALAR)
}

fn take_ext_payload(
    encoded: &[u8],
    position: &mut usize,
    payload_length: usize,
    marker_offset: usize,
) -> Result<(), StoredMsgpackError> {
    let length_with_tag = payload_length.checked_add(1).ok_or_else(|| {
        StoredMsgpackError::new(
            StoredMsgpackErrorKind::LengthOverflow,
            marker_offset,
            u64::MAX,
            usize_to_u64_saturating(payload_length),
        )
    })?;
    take_payload(encoded, position, length_with_tag, marker_offset)
}

fn take_payload(
    encoded: &[u8],
    position: &mut usize,
    length: usize,
    marker_offset: usize,
) -> Result<(), StoredMsgpackError> {
    let remaining = encoded.len().saturating_sub(*position);
    if remaining < length {
        return Err(StoredMsgpackError::new(
            StoredMsgpackErrorKind::Truncated,
            marker_offset,
            usize_to_u64_saturating(length),
            usize_to_u64_saturating(remaining),
        ));
    }
    *position = position.checked_add(length).ok_or_else(|| {
        StoredMsgpackError::new(
            StoredMsgpackErrorKind::LengthOverflow,
            marker_offset,
            u64::MAX,
            usize_to_u64_saturating(length),
        )
    })?;
    Ok(())
}

fn take_u8(
    encoded: &[u8],
    position: &mut usize,
    marker_offset: usize,
) -> Result<u8, StoredMsgpackError> {
    let Some(&value) = encoded.get(*position) else {
        return Err(StoredMsgpackError::new(
            StoredMsgpackErrorKind::Truncated,
            marker_offset,
            1,
            0,
        ));
    };
    *position += 1;
    Ok(value)
}

fn take_u16(
    encoded: &[u8],
    position: &mut usize,
    marker_offset: usize,
) -> Result<u16, StoredMsgpackError> {
    let bytes = take_array::<2>(encoded, position, marker_offset)?;
    Ok(u16::from_be_bytes(bytes))
}

fn take_u32(
    encoded: &[u8],
    position: &mut usize,
    marker_offset: usize,
) -> Result<u32, StoredMsgpackError> {
    let bytes = take_array::<4>(encoded, position, marker_offset)?;
    Ok(u32::from_be_bytes(bytes))
}

fn take_array<const N: usize>(
    encoded: &[u8],
    position: &mut usize,
    marker_offset: usize,
) -> Result<[u8; N], StoredMsgpackError> {
    let remaining = encoded.len().saturating_sub(*position);
    let Some(bytes) = encoded.get(*position..position.saturating_add(N)) else {
        return Err(StoredMsgpackError::new(
            StoredMsgpackErrorKind::Truncated,
            marker_offset,
            N as u64,
            usize_to_u64_saturating(remaining),
        ));
    };
    let value = bytes.try_into().map_err(|_| {
        StoredMsgpackError::new(
            StoredMsgpackErrorKind::Truncated,
            marker_offset,
            N as u64,
            usize_to_u64_saturating(remaining),
        )
    })?;
    *position += N;
    Ok(value)
}

fn usize_from_u32(value: u32, offset: usize) -> Result<usize, StoredMsgpackError> {
    usize::try_from(value).map_err(|_| {
        StoredMsgpackError::new(
            StoredMsgpackErrorKind::LengthOverflow,
            offset,
            usize::MAX as u64,
            u64::from(value),
        )
    })
}

const fn usize_to_u64_saturating(value: usize) -> u64 {
    if usize::BITS > u64::BITS && value > u64::MAX as usize {
        u64::MAX
    } else {
        value as u64
    }
}

const fn root_requirement_code(requirement: RootRequirement) -> u64 {
    match requirement {
        RootRequirement::Array => 1,
        RootRequirement::ArrayOrMap => 2,
    }
}

const fn root_kind_code(kind: RootKind) -> u64 {
    match kind {
        RootKind::Scalar => 0,
        RootKind::Array => 1,
        RootKind::Map => 2,
    }
}

#[cfg(test)]
mod policy_fingerprint_tests {
    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};

    use super::*;

    const COMPACT_STRUCT_KAT_V1: &[u8] = &[0x92, 0x07, 0xc3];
    const STRUCT_MAP_KAT_V1: &[u8] = &[0x82, 0xa1, b'a', 0x07, 0xa1, b'b', 0xc3];
    const ROW_ARRAY_KAT_V1: &[u8] = &[0x93, 0x01, 0xcc, 0x80, 0xcd, 0x01, 0x00];

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    struct CodecKat {
        a: u8,
        b: bool,
    }

    fn update_field(hasher: &mut Sha256, field_count: &mut u64, label: &[u8], value: &[u8]) {
        *field_count = field_count.checked_add(1).unwrap();
        hasher.update([0x01]);
        hasher.update(u64::try_from(label.len()).unwrap().to_be_bytes());
        hasher.update(label);
        hasher.update(u64::try_from(value.len()).unwrap().to_be_bytes());
        hasher.update(value);
    }

    fn exact_codec_kats() {
        let value = CodecKat { a: 7, b: true };
        let compact = rmp_serde::to_vec(&value).unwrap();
        let struct_map = rmp_serde::to_vec_named(&value).unwrap();
        assert_eq!(compact, COMPACT_STRUCT_KAT_V1);
        assert_eq!(struct_map, STRUCT_MAP_KAT_V1);
        require_canonical(&value, &compact, CanonicalMode::Compact).unwrap();
        require_canonical(&value, &struct_map, CanonicalMode::StructMap).unwrap();

        let row = vec![1_u64, 128, 256];
        let encoded_row = rmp_serde::to_vec(&row).unwrap();
        assert_eq!(encoded_row, ROW_ARRAY_KAT_V1);
        assert_eq!(
            decode_exact::<Vec<u64>>(&encoded_row, StoredMsgpackProfile::Row).unwrap(),
            row
        );
        let nonminimal = [0x93, 0xcc, 0x01, 0xcc, 0x80, 0xcd, 0x01, 0x00];
        assert!(require_canonical(&row, &nonminimal, CanonicalMode::Compact).is_err());
    }

    fn derive_exact_codec_policy_fingerprint_v1() -> [u8; 32] {
        exact_codec_kats();
        let mut hasher = Sha256::new();
        hasher.update(STORED_MSGPACK_EXACT_CODEC_POLICY_FINGERPRINT_DOMAIN_V1);
        let mut field_count = 0_u64;
        for (label, value) in [
            (
                b"policy-description".as_slice(),
                STORED_MSGPACK_EXACT_CODEC_POLICY_DESCRIPTION_V1.as_bytes(),
            ),
            (b"compact-struct-kat".as_slice(), COMPACT_STRUCT_KAT_V1),
            (b"struct-map-kat".as_slice(), STRUCT_MAP_KAT_V1),
            (b"row-array-kat".as_slice(), ROW_ARRAY_KAT_V1),
        ] {
            update_field(&mut hasher, &mut field_count, label, value);
        }
        update_field(
            &mut hasher,
            &mut field_count,
            b"scanner-stack-capacity",
            &(SCANNER_STACK_CAPACITY as u64).to_be_bytes(),
        );
        update_field(
            &mut hasher,
            &mut field_count,
            b"serde-depth-headroom",
            &2_u64.to_be_bytes(),
        );
        hasher.update([0xff]);
        hasher.update(field_count.to_be_bytes());
        hasher.finalize().into()
    }

    fn derive_profile_policy_fingerprint_v1(profile: StoredMsgpackProfile) -> [u8; 32] {
        let limits = profile.limits();
        let (name, root) = match profile {
            StoredMsgpackProfile::Row => (b"row".as_slice(), b"array".as_slice()),
            StoredMsgpackProfile::RelationCatalog => {
                (b"relation-catalog".as_slice(), b"array-or-map".as_slice())
            }
        };
        let mut hasher = Sha256::new();
        hasher.update(STORED_MSGPACK_PROFILE_POLICY_FINGERPRINT_DOMAIN_V1);
        let mut field_count = 0_u64;
        update_field(&mut hasher, &mut field_count, b"profile", name);
        update_field(
            &mut hasher,
            &mut field_count,
            b"exact-codec-policy",
            &STORED_MSGPACK_EXACT_CODEC_POLICY_FINGERPRINT_V1,
        );
        update_field(&mut hasher, &mut field_count, b"root", root);
        for (label, value) in [
            (b"bytes".as_slice(), limits.bytes as u64),
            (b"depth".as_slice(), limits.depth as u64),
            (b"tokens".as_slice(), limits.tokens),
            (b"containers".as_slice(), limits.containers),
            (
                b"items-per-container".as_slice(),
                limits.items_per_container,
            ),
        ] {
            update_field(&mut hasher, &mut field_count, label, &value.to_be_bytes());
        }
        hasher.update([0xff]);
        hasher.update(field_count.to_be_bytes());
        hasher.finalize().into()
    }

    #[test]
    fn stored_msgpack_policy_fingerprints_are_hard_pinned_literal_oracles() {
        assert_eq!(
            derive_exact_codec_policy_fingerprint_v1(),
            STORED_MSGPACK_EXACT_CODEC_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            derive_profile_policy_fingerprint_v1(StoredMsgpackProfile::Row),
            STORED_MSGPACK_ROW_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            derive_profile_policy_fingerprint_v1(StoredMsgpackProfile::RelationCatalog),
            STORED_MSGPACK_RELATION_CATALOG_POLICY_FINGERPRINT_V1
        );
        assert_ne!(STORED_MSGPACK_EXACT_CODEC_POLICY_FINGERPRINT_V1, [0; 32]);
        assert_ne!(STORED_MSGPACK_ROW_POLICY_FINGERPRINT_V1, [0; 32]);
        assert_ne!(
            STORED_MSGPACK_RELATION_CATALOG_POLICY_FINGERPRINT_V1,
            [0; 32]
        );
    }
}

//! Pure domain contracts for managed storage lineage and read-only snapshots.
//!
//! This module deliberately stops before managed mutation. In particular, a
//! [`MutationEpoch`] is only a database-generation MVCC revision: it is not an
//! authentication token, does not make filesystem bodies atomic with the graph,
//! and does not replace exact per-record compare-and-swap checks.
//!
//! The current durable relation inventory is not wholly owned by `mneme-core`:
//! adapter relations still include vector/search/tag projections and retry
//! ledgers. Defining a supposedly complete record enum here would therefore be
//! false. The snapshot boundary below carries bounded, strictly ordered
//! commitments to adapter-canonical records. It makes local page bounds,
//! ordering, duplication, continuation, and serialization falsifiable, but it
//! cannot authorize bind or apply until a later contract supplies and verifies
//! a closed canonical record inventory. Cross-page chaining, authentication,
//! and claims of whole-inventory completeness belong to the native snapshot
//! artifact, not to a caller-deserializable cursor.
//!
//! The managed text visitors borrow strings when the input format permits it
//! and reject out-of-bound text before decoding. Escaped or streaming JSON
//! may still allocate while the format constructs a string for Serde; C3's
//! native artifact loader must therefore enforce a file/frame-size cap before
//! deserialization. These field bounds are defense in depth, not that host fuse.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::fmt;
use std::mem::size_of;
use ulid::Ulid;

/// Largest signed storage integer accepted by the persistent adapters.
///
/// Keeping generations and epochs in this common range means
/// a value accepted by the reference domain can be represented by SQLite/Cozo
/// without a lossy `u64 -> i64` conversion.
pub const MAX_MANAGED_STORAGE_INTEGER: u64 = i64::MAX as u64;

/// Maximum number of record commitments returned in one snapshot page.
pub const MAX_MANAGED_SNAPSHOT_PAGE_RECORDS: usize = 256;

/// Conservative maximum expansion of one raw string-component byte under a
/// future order-preserving escape encoding.
const MANAGED_SNAPSHOT_KEY_ESCAPE_EXPANSION: usize = 2;

/// Reserved type-tag, component-terminator, and length-framing headroom.
const MANAGED_SNAPSHOT_KEY_FRAMING_HEADROOM_BYTES: usize = 64;

/// Raw bytes in the largest currently legal durable composite primary key:
/// `feedback_retry_order(epoch, sequence, key)`.
const MAX_MANAGED_SNAPSHOT_SOURCE_KEY_BYTES: usize =
    crate::MAX_FEEDBACK_EPOCH_BYTES + size_of::<u64>() + crate::MAX_FEEDBACK_BATCH_KEY_BYTES;

/// Maximum primary-key component inside [`ManagedSnapshotPosition`].
///
/// C2 still owns the actual typed, order-preserving tuple encoding. Until that
/// encoding is frozen, this C1a ceiling conservatively permits every variable
/// string byte to escape to two bytes, keeps the sequence fixed-width, and
/// reserves separate framing headroom. This formula is a capacity proof
/// obligation for C2, not an encoding specification.
pub const MAX_MANAGED_SNAPSHOT_PRIMARY_KEY_BYTES: usize = (MAX_MANAGED_SNAPSHOT_SOURCE_KEY_BYTES
    - size_of::<u64>())
    * MANAGED_SNAPSHOT_KEY_ESCAPE_EXPANSION
    + size_of::<u64>()
    + MANAGED_SNAPSHOT_KEY_FRAMING_HEADROOM_BYTES;

/// Maximum canonical position bytes for `(relation-order, primary-key)`.
///
/// These bytes are ordering data, not record authority.
pub const MAX_MANAGED_SNAPSHOT_POSITION_BYTES: usize =
    size_of::<u16>() + MAX_MANAGED_SNAPSHOT_PRIMARY_KEY_BYTES;

// Page digests length-frame positions with a `u16`; fail at compile time if a
// future domain expansion outgrows that representation instead of truncating.
const _: () = assert!(MAX_MANAGED_SNAPSHOT_POSITION_BYTES <= u16::MAX as usize);

const DIGEST_BYTES: usize = 32;
const DIGEST_HEX_BYTES: usize = DIGEST_BYTES * 2;
const HEAD_DIGEST_DOMAIN: &[u8] = b"mneme-managed-head-v1\0";
const PAGE_DIGEST_DOMAIN: &[u8] = b"mneme-managed-snapshot-page-v1\0";

/// Why a managed-domain value or snapshot page was rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ManagedDomainError {
    #[error("database id must not be the nil ULID")]
    NilDatabaseId,
    #[error("storage id must not be the nil ULID")]
    NilStorageId,
    #[error("storage generation must be in 1..={MAX_MANAGED_STORAGE_INTEGER}")]
    InvalidStorageGeneration,
    #[error("managed schema version must be positive")]
    InvalidManagedSchemaVersion,
    #[error("minimum writer schema version must be positive")]
    InvalidWriterSchemaVersion,
    #[error("mutation epoch must be in 0..={MAX_MANAGED_STORAGE_INTEGER}")]
    InvalidMutationEpoch,
    #[error("{field} has no representable successor")]
    Overflow { field: &'static str },
    #[error("snapshot page limit must be in 1..={MAX_MANAGED_SNAPSHOT_PAGE_RECORDS}, got {actual}")]
    InvalidPageLimit { actual: usize },
    #[error("snapshot digest must be exactly 64 lowercase hexadecimal bytes")]
    InvalidDigest,
    #[error("snapshot relation ordinal must be positive")]
    InvalidSnapshotRelation,
    #[error(
        "snapshot primary key must make the canonical position 3..={MAX_MANAGED_SNAPSHOT_POSITION_BYTES} bytes"
    )]
    InvalidSnapshotPrimaryKey,
    #[error("snapshot cursor belongs to a different store head")]
    CursorHeadMismatch,
    #[error("snapshot page returned {actual} records for a requested limit of {limit}")]
    PageExceedsRequest { actual: usize, limit: usize },
    #[error("snapshot continuation page must contain at least one record")]
    EmptyContinuation,
    #[error("snapshot positions must be in strictly increasing canonical order")]
    NonCanonicalSnapshotOrder,
    #[error("snapshot page contains duplicate position {position}")]
    DuplicateSnapshotPosition { position: ManagedSnapshotPosition },
    #[error("snapshot page contains an invalid derived cursor or digest")]
    InvalidPageCommitment,
}

/// Stable logical database identity.
///
/// This wraps the existing persisted `Ulid`; it does not introduce another
/// lineage identifier. The nil ULID is reserved for missing/corrupt metadata.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DatabaseId(Ulid);

impl DatabaseId {
    pub fn new(value: Ulid) -> Result<Self, ManagedDomainError> {
        if value == Ulid::from(0_u128) {
            return Err(ManagedDomainError::NilDatabaseId);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> Ulid {
        self.0
    }
}

impl TryFrom<Ulid> for DatabaseId {
    type Error = ManagedDomainError;

    fn try_from(value: Ulid) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<DatabaseId> for Ulid {
    fn from(value: DatabaseId) -> Self {
        value.get()
    }
}

impl fmt::Debug for DatabaseId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("DatabaseId").field(&self.0).finish()
    }
}

impl fmt::Display for DatabaseId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for DatabaseId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DatabaseId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(deserialize_ulid(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Identity of one physical storage generation.
///
/// A managed operation may use its operation ULID as this value. It is distinct
/// from [`DatabaseId`], which remains stable across generation replacement.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StorageId(Ulid);

impl StorageId {
    pub fn new(value: Ulid) -> Result<Self, ManagedDomainError> {
        if value == Ulid::from(0_u128) {
            return Err(ManagedDomainError::NilStorageId);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> Ulid {
        self.0
    }
}

impl TryFrom<Ulid> for StorageId {
    type Error = ManagedDomainError;

    fn try_from(value: Ulid) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<StorageId> for Ulid {
    fn from(value: StorageId) -> Self {
        value.get()
    }
}

impl fmt::Debug for StorageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("StorageId").field(&self.0).finish()
    }
}

impl fmt::Display for StorageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for StorageId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StorageId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(deserialize_ulid(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Deserialize the ULID string wire shape without `ulid`'s unconditional
/// intermediate `String` allocation.
fn deserialize_ulid<'de, D>(deserializer: D) -> Result<Ulid, D::Error>
where
    D: Deserializer<'de>,
{
    struct Visitor;

    impl Visitor {
        fn parse<E>(value: &str) -> Result<Ulid, E>
        where
            E: serde::de::Error,
        {
            if value.len() != ulid::ULID_LEN {
                return Err(E::invalid_length(value.len(), &Self));
            }
            Ulid::from_string(value).map_err(E::custom)
        }
    }

    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = Ulid;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "a {}-byte ULID string", ulid::ULID_LEN)
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Self::parse(value)
        }

        fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Self::parse(value)
        }
    }

    deserializer.deserialize_str(Visitor)
}

/// Monotonic position of one physical storage generation in a database lineage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct StorageGeneration(u64);

impl StorageGeneration {
    pub fn new(value: u64) -> Result<Self, ManagedDomainError> {
        if value == 0 || value > MAX_MANAGED_STORAGE_INTEGER {
            return Err(ManagedDomainError::InvalidStorageGeneration);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub fn checked_successor(self) -> Result<Self, ManagedDomainError> {
        let value = self
            .0
            .checked_add(1)
            .filter(|value| *value <= MAX_MANAGED_STORAGE_INTEGER)
            .ok_or(ManagedDomainError::Overflow {
                field: "storage generation",
            })?;
        Ok(Self(value))
    }
}

impl<'de> Deserialize<'de> for StorageGeneration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Version of the managed relations represented by one physical store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ManagedSchemaVersion(u32);

impl ManagedSchemaVersion {
    pub fn new(value: u32) -> Result<Self, ManagedDomainError> {
        if value == 0 {
            return Err(ManagedDomainError::InvalidManagedSchemaVersion);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

impl<'de> Deserialize<'de> for ManagedSchemaVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u32::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Oldest writer-contract version permitted to mutate a managed store.
///
/// This is a distinct version axis from [`ManagedSchemaVersion`]. A newer
/// writer can understand an older managed schema, so comparing their numeric
/// values or interpreting them as one inclusive range would be bogus. The
/// adapter performs exact compatibility checks at open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct WriterSchemaVersion(u32);

impl WriterSchemaVersion {
    pub fn new(value: u32) -> Result<Self, ManagedDomainError> {
        if value == 0 {
            return Err(ManagedDomainError::InvalidWriterSchemaVersion);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

impl<'de> Deserialize<'de> for WriterSchemaVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u32::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Database-wide MVCC revision of one physical storage generation.
///
/// Epoch zero is the valid initial revision. A successful durable mutation must
/// install exactly one checked successor in the same database transaction.
/// This value says nothing about snapshot authentication or external body
/// atomicity and is never sufficient authorization for a managed write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct MutationEpoch(u64);

impl MutationEpoch {
    pub fn new(value: u64) -> Result<Self, ManagedDomainError> {
        if value > MAX_MANAGED_STORAGE_INTEGER {
            return Err(ManagedDomainError::InvalidMutationEpoch);
        }
        Ok(Self(value))
    }

    pub const fn initial() -> Self {
        Self(0)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub fn checked_successor(self) -> Result<Self, ManagedDomainError> {
        let value = self
            .0
            .checked_add(1)
            .filter(|value| *value <= MAX_MANAGED_STORAGE_INTEGER)
            .ok_or(ManagedDomainError::Overflow {
                field: "mutation epoch",
            })?;
        Ok(Self(value))
    }
}

impl<'de> Deserialize<'de> for MutationEpoch {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Identity of one managed physical storage generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StorageIdentity {
    id: StorageId,
    generation: StorageGeneration,
}

impl StorageIdentity {
    pub const fn new(id: StorageId, generation: StorageGeneration) -> Self {
        Self { id, generation }
    }

    pub const fn id(self) -> StorageId {
        self.id
    }

    pub const fn generation(self) -> StorageGeneration {
        self.generation
    }
}

impl<'de> Deserialize<'de> for StorageIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            id: StorageId,
            generation: StorageGeneration,
        }

        let wire = Wire::deserialize(deserializer)?;
        Ok(Self::new(wire.id, wire.generation))
    }
}

/// Exact read-only identity observed at the head of a store.
///
/// An unmanaged head has a logical database identity but makes no storage,
/// generation, writer-fence, or epoch claim. Equality therefore cannot detect
/// conventional graph mutation. Exact unmanaged paging requires an immutable
/// adapter/native snapshot or lease-owned backup. Managed equality includes the
/// mutation epoch and can detect managed-store drift.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedStoreHead {
    Unmanaged {
        database_id: DatabaseId,
    },
    Managed {
        database_id: DatabaseId,
        storage: StorageIdentity,
        managed_schema_version: ManagedSchemaVersion,
        minimum_writer_schema: WriterSchemaVersion,
        mutation_epoch: MutationEpoch,
    },
}

impl ManagedStoreHead {
    pub const fn unmanaged(database_id: DatabaseId) -> Self {
        Self::Unmanaged { database_id }
    }

    pub const fn managed(
        database_id: DatabaseId,
        storage: StorageIdentity,
        managed_schema_version: ManagedSchemaVersion,
        minimum_writer_schema: WriterSchemaVersion,
        mutation_epoch: MutationEpoch,
    ) -> Self {
        Self::Managed {
            database_id,
            storage,
            managed_schema_version,
            minimum_writer_schema,
            mutation_epoch,
        }
    }

    pub const fn database_id(self) -> DatabaseId {
        match self {
            Self::Unmanaged { database_id } | Self::Managed { database_id, .. } => database_id,
        }
    }

    pub const fn storage(self) -> Option<StorageIdentity> {
        match self {
            Self::Unmanaged { .. } => None,
            Self::Managed { storage, .. } => Some(storage),
        }
    }

    pub const fn managed_schema_version(self) -> Option<ManagedSchemaVersion> {
        match self {
            Self::Unmanaged { .. } => None,
            Self::Managed {
                managed_schema_version,
                ..
            } => Some(managed_schema_version),
        }
    }

    pub const fn minimum_writer_schema(self) -> Option<WriterSchemaVersion> {
        match self {
            Self::Unmanaged { .. } => None,
            Self::Managed {
                minimum_writer_schema,
                ..
            } => Some(minimum_writer_schema),
        }
    }

    pub const fn mutation_epoch(self) -> Option<MutationEpoch> {
        match self {
            Self::Unmanaged { .. } => None,
            Self::Managed { mutation_epoch, .. } => Some(mutation_epoch),
        }
    }

    pub const fn is_managed(self) -> bool {
        matches!(self, Self::Managed { .. })
    }

    /// Stable domain-separated digest used only to bind cursors to this
    /// observed head. Managed bindings include the mutation epoch; unmanaged
    /// bindings contain only the logical database id and cannot prove snapshot
    /// consistency. This is not an authentication code.
    pub fn cursor_binding(self) -> ManagedSnapshotDigest {
        let mut hash = Sha256::new();
        hash.update(HEAD_DIGEST_DOMAIN);
        match self {
            Self::Unmanaged { database_id } => {
                hash.update([0]);
                update_ulid(&mut hash, database_id.get());
            }
            Self::Managed {
                database_id,
                storage,
                managed_schema_version,
                minimum_writer_schema,
                mutation_epoch,
            } => {
                hash.update([1]);
                update_ulid(&mut hash, database_id.get());
                update_ulid(&mut hash, storage.id().get());
                hash.update(storage.generation().get().to_be_bytes());
                hash.update(managed_schema_version.get().to_be_bytes());
                hash.update(minimum_writer_schema.get().to_be_bytes());
                hash.update(mutation_epoch.get().to_be_bytes());
            }
        }
        ManagedSnapshotDigest(hash.finalize().into())
    }
}

impl<'de> Deserialize<'de> for ManagedStoreHead {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
        enum Wire {
            Unmanaged {
                database_id: DatabaseId,
            },
            Managed {
                database_id: DatabaseId,
                storage: StorageIdentity,
                managed_schema_version: ManagedSchemaVersion,
                minimum_writer_schema: WriterSchemaVersion,
                mutation_epoch: MutationEpoch,
            },
        }

        Ok(match Wire::deserialize(deserializer)? {
            Wire::Unmanaged { database_id } => Self::unmanaged(database_id),
            Wire::Managed {
                database_id,
                storage,
                managed_schema_version,
                minimum_writer_schema,
                mutation_epoch,
            } => Self::managed(
                database_id,
                storage,
                managed_schema_version,
                minimum_writer_schema,
                mutation_epoch,
            ),
        })
    }
}

/// Lowercase SHA-256 commitment used by the snapshot protocol.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ManagedSnapshotDigest([u8; DIGEST_BYTES]);

impl ManagedSnapshotDigest {
    pub const fn from_bytes(bytes: [u8; DIGEST_BYTES]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; DIGEST_BYTES] {
        &self.0
    }

    pub fn parse(value: &str) -> Result<Self, ManagedDomainError> {
        decode_digest(value, ManagedDomainError::InvalidDigest).map(Self)
    }
}

impl fmt::Debug for ManagedSnapshotDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ManagedSnapshotDigest")
            .field(&Hex(&self.0))
            .finish()
    }
}

impl fmt::Display for ManagedSnapshotDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        Hex(&self.0).fmt(formatter)
    }
}

impl Serialize for ManagedSnapshotDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ManagedSnapshotDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ManagedSnapshotDigest;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "exactly {DIGEST_HEX_BYTES} lowercase hexadecimal bytes"
                )
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                ManagedSnapshotDigest::parse(value).map_err(E::custom)
            }

            fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                ManagedSnapshotDigest::parse(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

/// Bounded canonical `(relation-order, primary-key)` position.
///
/// C2 owns the closed relation vocabulary and an order-preserving primary-key
/// encoding. Keeping those canonical bytes here (instead of sorting SHA-256 key
/// hashes) lets an adapter continue with its existing relation/key indexes and
/// bounded working memory. The position is pagination data only: it neither
/// contains a record value nor authorizes a snapshot/apply operation.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ManagedSnapshotPosition(Box<[u8]>);

impl ManagedSnapshotPosition {
    pub fn new(relation: u16, primary_key: impl AsRef<[u8]>) -> Result<Self, ManagedDomainError> {
        if relation == 0 {
            return Err(ManagedDomainError::InvalidSnapshotRelation);
        }
        let primary_key = primary_key.as_ref();
        if primary_key.is_empty() || primary_key.len() > MAX_MANAGED_SNAPSHOT_PRIMARY_KEY_BYTES {
            return Err(ManagedDomainError::InvalidSnapshotPrimaryKey);
        }
        let mut bytes = Vec::with_capacity(size_of::<u16>() + primary_key.len());
        bytes.extend_from_slice(&relation.to_be_bytes());
        bytes.extend_from_slice(primary_key);
        Ok(Self(bytes.into_boxed_slice()))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn relation(&self) -> u16 {
        u16::from_be_bytes(
            self.0[..size_of::<u16>()]
                .try_into()
                .expect("validated snapshot positions contain a relation ordinal"),
        )
    }

    pub fn primary_key(&self) -> &[u8] {
        &self.0[size_of::<u16>()..]
    }

    pub fn parse(value: &str) -> Result<Self, ManagedDomainError> {
        if value.len() < 6
            || !value.len().is_multiple_of(2)
            || value.len() > MAX_MANAGED_SNAPSHOT_POSITION_BYTES * 2
        {
            return Err(ManagedDomainError::InvalidSnapshotPrimaryKey);
        }
        let mut bytes = Vec::with_capacity(value.len() / 2);
        for pair in value.as_bytes().chunks_exact(2) {
            let high =
                lower_hex_nibble(pair[0]).ok_or(ManagedDomainError::InvalidSnapshotPrimaryKey)?;
            let low =
                lower_hex_nibble(pair[1]).ok_or(ManagedDomainError::InvalidSnapshotPrimaryKey)?;
            bytes.push((high << 4) | low);
        }
        let relation = u16::from_be_bytes(
            bytes[..size_of::<u16>()]
                .try_into()
                .expect("minimum encoded position length checked above"),
        );
        Self::new(relation, &bytes[size_of::<u16>()..])
    }
}

impl fmt::Debug for ManagedSnapshotPosition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ManagedSnapshotPosition")
            .field(&Hex(&self.0))
            .finish()
    }
}

impl fmt::Display for ManagedSnapshotPosition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        Hex(&self.0).fmt(formatter)
    }
}

impl Serialize for ManagedSnapshotPosition {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ManagedSnapshotPosition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ManagedSnapshotPosition;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "6..={} lowercase hexadecimal bytes containing a positive relation and bounded primary key",
                    MAX_MANAGED_SNAPSHOT_POSITION_BYTES * 2
                )
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                ManagedSnapshotPosition::parse(value).map_err(E::custom)
            }

            fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                ManagedSnapshotPosition::parse(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

/// Commitment to one complete adapter-canonical durable record.
///
/// The record digest must domain-separate and cover the relation, primary key,
/// and complete canonical value. This type does not claim that the adapter's
/// record vocabulary is complete; C2 must establish that inventory before these
/// commitments can become apply evidence.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSnapshotRecordCommitment {
    position: ManagedSnapshotPosition,
    digest: ManagedSnapshotDigest,
}

impl ManagedSnapshotRecordCommitment {
    pub const fn new(position: ManagedSnapshotPosition, digest: ManagedSnapshotDigest) -> Self {
        Self { position, digest }
    }

    pub fn position(&self) -> &ManagedSnapshotPosition {
        &self.position
    }

    pub const fn digest(&self) -> ManagedSnapshotDigest {
        self.digest
    }
}

/// Validated caller page bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ManagedSnapshotPageLimit(u16);

impl ManagedSnapshotPageLimit {
    pub fn new(value: usize) -> Result<Self, ManagedDomainError> {
        if value == 0 || value > MAX_MANAGED_SNAPSHOT_PAGE_RECORDS {
            return Err(ManagedDomainError::InvalidPageLimit { actual: value });
        }
        Ok(Self(value as u16))
    }

    pub const fn get(self) -> usize {
        self.0 as usize
    }
}

impl<'de> Deserialize<'de> for ManagedSnapshotPageLimit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(usize::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Opaque validated continuation for one exact observed store head.
///
/// Deserialization proves only shape and head binding. A managed binding
/// includes the mutation epoch, while an unmanaged binding contains only the
/// logical database id and cannot detect conventional graph mutation. A cursor
/// is not proof that all earlier positions were read; a completeness-claiming
/// native caller must page an immutable source, start at `None`, and follow only
/// cursors returned by prior pages.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSnapshotCursor {
    head_binding: ManagedSnapshotDigest,
    after: ManagedSnapshotPosition,
}

impl ManagedSnapshotCursor {
    fn new(head_binding: ManagedSnapshotDigest, after: ManagedSnapshotPosition) -> Self {
        Self {
            head_binding,
            after,
        }
    }

    pub const fn head_binding(&self) -> ManagedSnapshotDigest {
        self.head_binding
    }

    pub fn after(&self) -> &ManagedSnapshotPosition {
        &self.after
    }
}

impl<'de> Deserialize<'de> for ManagedSnapshotCursor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            head_binding: ManagedSnapshotDigest,
            after: ManagedSnapshotPosition,
        }

        let wire = Wire::deserialize(deserializer)?;
        Ok(Self::new(wire.head_binding, wire.after))
    }
}

/// One validated request for a bounded snapshot commitment page.
///
/// For an unmanaged head, this request does not itself establish a stable read
/// boundary. The native publisher must serve it from an immutable adapter
/// snapshot or lease-owned backup rather than a mutating conventional store.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSnapshotRequest {
    head: ManagedStoreHead,
    after: Option<ManagedSnapshotCursor>,
    limit: ManagedSnapshotPageLimit,
}

impl ManagedSnapshotRequest {
    pub fn new(
        head: ManagedStoreHead,
        after: Option<ManagedSnapshotCursor>,
        limit: ManagedSnapshotPageLimit,
    ) -> Result<Self, ManagedDomainError> {
        if after
            .as_ref()
            .is_some_and(|cursor| cursor.head_binding() != head.cursor_binding())
        {
            return Err(ManagedDomainError::CursorHeadMismatch);
        }
        Ok(Self { head, after, limit })
    }

    pub const fn head(&self) -> ManagedStoreHead {
        self.head
    }

    pub const fn after(&self) -> Option<&ManagedSnapshotCursor> {
        self.after.as_ref()
    }

    pub const fn limit(&self) -> ManagedSnapshotPageLimit {
        self.limit
    }
}

impl<'de> Deserialize<'de> for ManagedSnapshotRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            head: ManagedStoreHead,
            after: Option<ManagedSnapshotCursor>,
            limit: ManagedSnapshotPageLimit,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.head, wire.after, wire.limit).map_err(serde::de::Error::custom)
    }
}

/// One bounded, strictly ordered page of canonical-record commitments.
///
/// `page_digest` commits only to this local response and its exclusive `after`
/// position. It is not a cross-page chain, whole-inventory digest, or
/// authentication code. The native snapshot publisher must start at `None`,
/// follow only returned cursors, establish the closed C2 relation inventory,
/// and authenticate its ordered page hashes before making a completeness claim.
/// For an unmanaged head, that publisher must page an immutable adapter snapshot
/// or lease-owned backup because head equality cannot detect conventional graph
/// mutation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSnapshotPage {
    head: ManagedStoreHead,
    after: Option<ManagedSnapshotCursor>,
    limit: ManagedSnapshotPageLimit,
    records: Vec<ManagedSnapshotRecordCommitment>,
    has_more: bool,
    next: Option<ManagedSnapshotCursor>,
    page_digest: ManagedSnapshotDigest,
}

impl ManagedSnapshotPage {
    pub fn new(
        request: &ManagedSnapshotRequest,
        records: Vec<ManagedSnapshotRecordCommitment>,
        has_more: bool,
    ) -> Result<Self, ManagedDomainError> {
        if records.len() > request.limit().get() {
            return Err(ManagedDomainError::PageExceedsRequest {
                actual: records.len(),
                limit: request.limit().get(),
            });
        }
        if has_more && records.is_empty() {
            return Err(ManagedDomainError::EmptyContinuation);
        }

        let mut previous = request.after().map(ManagedSnapshotCursor::after);
        for record in &records {
            if let Some(previous) = previous {
                if record.position() == previous {
                    return Err(ManagedDomainError::DuplicateSnapshotPosition {
                        position: record.position().clone(),
                    });
                }
                if record.position() < previous {
                    return Err(ManagedDomainError::NonCanonicalSnapshotOrder);
                }
            }
            previous = Some(record.position());
        }

        let next = if has_more {
            let after = records
                .last()
                .expect("non-empty continuation page checked above")
                .position()
                .clone();
            Some(ManagedSnapshotCursor::new(
                request.head().cursor_binding(),
                after,
            ))
        } else {
            None
        };
        let page_digest = snapshot_page_digest(request, &records, has_more);

        Ok(Self {
            head: request.head(),
            after: request.after().cloned(),
            limit: request.limit(),
            records,
            has_more,
            next,
            page_digest,
        })
    }

    pub const fn head(&self) -> ManagedStoreHead {
        self.head
    }

    pub const fn after(&self) -> Option<&ManagedSnapshotCursor> {
        self.after.as_ref()
    }

    pub const fn limit(&self) -> ManagedSnapshotPageLimit {
        self.limit
    }

    pub fn records(&self) -> &[ManagedSnapshotRecordCommitment] {
        &self.records
    }

    pub const fn has_more(&self) -> bool {
        self.has_more
    }

    pub const fn next(&self) -> Option<&ManagedSnapshotCursor> {
        self.next.as_ref()
    }

    pub const fn page_digest(&self) -> ManagedSnapshotDigest {
        self.page_digest
    }

    pub fn validate_against(
        &self,
        request: &ManagedSnapshotRequest,
    ) -> Result<(), ManagedDomainError> {
        let rebuilt = Self::new(request, self.records.clone(), self.has_more)?;
        if &rebuilt == self {
            Ok(())
        } else {
            Err(ManagedDomainError::InvalidPageCommitment)
        }
    }
}

impl<'de> Deserialize<'de> for ManagedSnapshotPage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            head: ManagedStoreHead,
            after: Option<ManagedSnapshotCursor>,
            limit: ManagedSnapshotPageLimit,
            records: BoundedSnapshotRecords,
            has_more: bool,
            next: Option<ManagedSnapshotCursor>,
            page_digest: ManagedSnapshotDigest,
        }

        let wire = Wire::deserialize(deserializer)?;
        let request = ManagedSnapshotRequest::new(wire.head, wire.after, wire.limit)
            .map_err(serde::de::Error::custom)?;
        let rebuilt =
            Self::new(&request, wire.records.0, wire.has_more).map_err(serde::de::Error::custom)?;
        if rebuilt.next != wire.next || rebuilt.page_digest != wire.page_digest {
            return Err(serde::de::Error::custom(
                ManagedDomainError::InvalidPageCommitment,
            ));
        }
        Ok(rebuilt)
    }
}

/// Deserialization-only guard that stops after one record beyond the hard page
/// bound instead of first allocating an attacker-sized `Vec` and rejecting it
/// afterward.
struct BoundedSnapshotRecords(Vec<ManagedSnapshotRecordCommitment>);

impl<'de> Deserialize<'de> for BoundedSnapshotRecords {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = BoundedSnapshotRecords;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "at most {MAX_MANAGED_SNAPSHOT_PAGE_RECORDS} snapshot record commitments"
                )
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let capacity = sequence
                    .size_hint()
                    .unwrap_or(0)
                    .min(MAX_MANAGED_SNAPSHOT_PAGE_RECORDS);
                let mut records = Vec::with_capacity(capacity);
                while let Some(record) = sequence.next_element()? {
                    if records.len() == MAX_MANAGED_SNAPSHOT_PAGE_RECORDS {
                        return Err(serde::de::Error::custom(
                            ManagedDomainError::PageExceedsRequest {
                                actual: MAX_MANAGED_SNAPSHOT_PAGE_RECORDS + 1,
                                limit: MAX_MANAGED_SNAPSHOT_PAGE_RECORDS,
                            },
                        ));
                    }
                    records.push(record);
                }
                Ok(BoundedSnapshotRecords(records))
            }
        }

        deserializer.deserialize_seq(Visitor)
    }
}

fn snapshot_page_digest(
    request: &ManagedSnapshotRequest,
    records: &[ManagedSnapshotRecordCommitment],
    has_more: bool,
) -> ManagedSnapshotDigest {
    let mut hash = Sha256::new();
    hash.update(PAGE_DIGEST_DOMAIN);
    hash.update(request.head().cursor_binding().as_bytes());
    match request.after() {
        Some(cursor) => {
            hash.update([1]);
            update_bounded_bytes(&mut hash, cursor.after().as_bytes());
        }
        None => hash.update([0]),
    }
    hash.update((request.limit().get() as u16).to_be_bytes());
    hash.update((records.len() as u16).to_be_bytes());
    for record in records {
        update_bounded_bytes(&mut hash, record.position().as_bytes());
        hash.update(record.digest().as_bytes());
    }
    hash.update([u8::from(has_more)]);
    ManagedSnapshotDigest(hash.finalize().into())
}

fn update_bounded_bytes(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u16).to_be_bytes());
    hash.update(value);
}

fn update_ulid(hash: &mut Sha256, value: Ulid) {
    hash.update(u128::from(value).to_be_bytes());
}

fn decode_digest(
    value: &str,
    error: ManagedDomainError,
) -> Result<[u8; DIGEST_BYTES], ManagedDomainError> {
    if value.len() != DIGEST_HEX_BYTES {
        return Err(error);
    }
    let mut bytes = [0_u8; DIGEST_BYTES];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = lower_hex_nibble(pair[0]).ok_or_else(|| error.clone())?;
        let low = lower_hex_nibble(pair[1]).ok_or_else(|| error.clone())?;
        bytes[index] = (high << 4) | low;
    }
    Ok(bytes)
}

const fn lower_hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

struct Hex<'a>(&'a [u8]);

impl fmt::Debug for Hex<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Display for Hex<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in self.0 {
            formatter.write_str(
                std::str::from_utf8(&[HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 0x0f)]])
                    .expect("hexadecimal bytes are valid UTF-8"),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn database_id(value: u128) -> DatabaseId {
        DatabaseId::new(Ulid::from(value)).unwrap()
    }

    fn storage_id(value: u128) -> StorageId {
        StorageId::new(Ulid::from(value)).unwrap()
    }

    fn managed_head(epoch: u64) -> ManagedStoreHead {
        ManagedStoreHead::managed(
            database_id(1),
            StorageIdentity::new(storage_id(2), StorageGeneration::new(3).unwrap()),
            ManagedSchemaVersion::new(4).unwrap(),
            WriterSchemaVersion::new(20).unwrap(),
            MutationEpoch::new(epoch).unwrap(),
        )
    }

    fn position(value: u8) -> ManagedSnapshotPosition {
        ManagedSnapshotPosition::new(1, [value]).unwrap()
    }

    fn digest(value: u8) -> ManagedSnapshotDigest {
        ManagedSnapshotDigest::from_bytes([value; DIGEST_BYTES])
    }

    fn record(value: u8) -> ManagedSnapshotRecordCommitment {
        ManagedSnapshotRecordCommitment::new(position(value), digest(value.wrapping_add(64)))
    }

    fn request(
        head: ManagedStoreHead,
        after: Option<ManagedSnapshotCursor>,
        limit: usize,
    ) -> ManagedSnapshotRequest {
        ManagedSnapshotRequest::new(head, after, ManagedSnapshotPageLimit::new(limit).unwrap())
            .unwrap()
    }

    /// Deserializer that proves a domain type asks Serde for a borrowable string
    /// instead of unconditionally requesting an owned `String`.
    struct BorrowedStrOnly<'de>(&'de str);

    impl<'de> serde::de::Deserializer<'de> for BorrowedStrOnly<'de> {
        type Error = serde::de::value::Error;

        fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
        where
            V: serde::de::Visitor<'de>,
        {
            visitor.visit_borrowed_str(self.0)
        }

        fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Self::Error>
        where
            V: serde::de::Visitor<'de>,
        {
            visitor.visit_borrowed_str(self.0)
        }

        fn deserialize_string<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
        where
            V: serde::de::Visitor<'de>,
        {
            Err(serde::de::Error::custom("owned string requested"))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char
            bytes byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct
            map struct enum identifier ignored_any
        }
    }

    #[test]
    fn ids_reject_nil_construction_and_deserialization() {
        assert_eq!(
            DatabaseId::new(Ulid::from(0_u128)),
            Err(ManagedDomainError::NilDatabaseId)
        );
        assert_eq!(
            StorageId::new(Ulid::from(0_u128)),
            Err(ManagedDomainError::NilStorageId)
        );

        let nil = serde_json::to_string(&Ulid::from(0_u128)).unwrap();
        assert!(serde_json::from_str::<DatabaseId>(&nil).is_err());
        assert!(serde_json::from_str::<StorageId>(&nil).is_err());
    }

    #[test]
    fn generation_bounds_successor_and_serde_are_checked() {
        assert!(StorageGeneration::new(0).is_err());
        assert!(StorageGeneration::new(MAX_MANAGED_STORAGE_INTEGER + 1).is_err());
        assert_eq!(
            StorageGeneration::new(7)
                .unwrap()
                .checked_successor()
                .unwrap()
                .get(),
            8
        );
        assert_eq!(
            StorageGeneration::new(MAX_MANAGED_STORAGE_INTEGER)
                .unwrap()
                .checked_successor(),
            Err(ManagedDomainError::Overflow {
                field: "storage generation"
            })
        );
        assert!(serde_json::from_str::<StorageGeneration>("0").is_err());
        assert!(
            serde_json::from_str::<StorageGeneration>(
                &(MAX_MANAGED_STORAGE_INTEGER + 1).to_string()
            )
            .is_err()
        );
    }

    #[test]
    fn managed_and_writer_schema_axes_validate_independently() {
        assert!(ManagedSchemaVersion::new(0).is_err());
        assert!(WriterSchemaVersion::new(0).is_err());
        assert_eq!(ManagedSchemaVersion::new(u32::MAX).unwrap().get(), u32::MAX);
        assert_eq!(WriterSchemaVersion::new(u32::MAX).unwrap().get(), u32::MAX);
        assert!(serde_json::from_str::<ManagedSchemaVersion>("0").is_err());
        assert!(serde_json::from_str::<WriterSchemaVersion>("0").is_err());

        // These are deliberately not ordered as one numeric range: writer v20
        // may understand managed schema v4.
        let head = managed_head(0);
        assert_eq!(head.managed_schema_version().unwrap().get(), 4);
        assert_eq!(head.minimum_writer_schema().unwrap().get(), 20);
    }

    #[test]
    fn mutation_epoch_is_bounded_and_checked_but_zero_is_valid() {
        assert_eq!(MutationEpoch::initial(), MutationEpoch::new(0).unwrap());
        assert!(MutationEpoch::new(MAX_MANAGED_STORAGE_INTEGER + 1).is_err());
        assert_eq!(
            MutationEpoch::new(MAX_MANAGED_STORAGE_INTEGER)
                .unwrap()
                .checked_successor(),
            Err(ManagedDomainError::Overflow {
                field: "mutation epoch"
            })
        );
        assert!(
            serde_json::from_str::<MutationEpoch>(&(MAX_MANAGED_STORAGE_INTEGER + 1).to_string())
                .is_err()
        );
    }

    #[test]
    fn managed_and_unmanaged_heads_cannot_mix_fields_through_serde() {
        let unmanaged = ManagedStoreHead::unmanaged(database_id(1));
        assert!(!unmanaged.is_managed());
        assert_eq!(unmanaged.storage(), None);
        assert_eq!(unmanaged.managed_schema_version(), None);
        assert_eq!(unmanaged.minimum_writer_schema(), None);
        assert_eq!(unmanaged.mutation_epoch(), None);

        let managed = managed_head(5);
        assert!(managed.is_managed());
        assert_eq!(managed.database_id(), database_id(1));
        assert_eq!(managed.mutation_epoch().unwrap().get(), 5);
        let round_trip: ManagedStoreHead =
            serde_json::from_str(&serde_json::to_string(&managed).unwrap()).unwrap();
        assert_eq!(round_trip, managed);

        let mut forged = serde_json::to_value(unmanaged).unwrap();
        forged["mutation_epoch"] = json!(4);
        assert!(serde_json::from_value::<ManagedStoreHead>(forged).is_err());

        let mut forged = serde_json::to_value(managed).unwrap();
        forged.as_object_mut().unwrap().remove("storage");
        assert!(serde_json::from_value::<ManagedStoreHead>(forged).is_err());
    }

    #[test]
    fn digest_and_position_require_canonical_bounded_encodings() {
        let encoded = "ab".repeat(DIGEST_BYTES);
        assert_eq!(
            ManagedSnapshotDigest::parse(&encoded).unwrap().to_string(),
            encoded
        );
        let encoded_position = "0001abcd";
        let parsed = ManagedSnapshotPosition::parse(encoded_position).unwrap();
        assert_eq!(parsed.to_string(), encoded_position);
        assert_eq!(parsed.relation(), 1);
        assert_eq!(parsed.primary_key(), &[0xab, 0xcd]);
        assert!(ManagedSnapshotDigest::parse(&"a".repeat(63)).is_err());
        assert!(ManagedSnapshotDigest::parse(&"AB".repeat(DIGEST_BYTES)).is_err());
        assert!(ManagedSnapshotPosition::new(0, [1]).is_err());
        assert!(ManagedSnapshotPosition::new(1, []).is_err());
        let maximum = vec![0; MAX_MANAGED_SNAPSHOT_PRIMARY_KEY_BYTES];
        assert_eq!(
            ManagedSnapshotPosition::new(1, &maximum)
                .unwrap()
                .as_bytes()
                .len(),
            MAX_MANAGED_SNAPSHOT_POSITION_BYTES
        );
        assert!(
            ManagedSnapshotPosition::new(1, vec![0; MAX_MANAGED_SNAPSHOT_POSITION_BYTES - 1])
                .is_err()
        );
        assert!(ManagedSnapshotPosition::parse("0000aa").is_err());
        assert!(ManagedSnapshotPosition::parse("0001g0").is_err());
        assert!(serde_json::from_value::<ManagedSnapshotPosition>(json!("0001")).is_err());
        assert!(serde_json::from_value::<ManagedSnapshotDigest>(json!("AB".repeat(32))).is_err());
    }

    #[test]
    fn maximum_feedback_retry_order_composite_key_fits_position_bound() {
        let retry = crate::ports::FeedbackRetryScope::new(
            "e".repeat(crate::MAX_FEEDBACK_EPOCH_BYTES),
            i64::MAX as u64 - 1,
            1,
        )
        .unwrap();
        let identity = crate::ports::FeedbackIdempotency::new(
            "k".repeat(crate::MAX_FEEDBACK_BATCH_KEY_BYTES),
            "a".repeat(crate::MAX_FEEDBACK_FINGERPRINT_BYTES),
            retry,
        )
        .unwrap();
        let sequence = identity.retry.sequence.to_be_bytes();
        let source_bytes = identity.retry.epoch.len() + sequence.len() + identity.key.len();
        assert_eq!(source_bytes, MAX_MANAGED_SNAPSHOT_SOURCE_KEY_BYTES);

        // This deliberately reserves a worst-case escaped tuple; it does not
        // define the encoding. C2 must freeze the typed order-preserving form
        // and prove its maximum legal value fits this C1a ceiling.
        let mut reserved_encoding = Vec::with_capacity(MAX_MANAGED_SNAPSHOT_PRIMARY_KEY_BYTES);
        for byte in identity.retry.epoch.bytes() {
            reserved_encoding.extend_from_slice(&[byte, 0]);
        }
        reserved_encoding.extend_from_slice(&sequence);
        for byte in identity.key.bytes() {
            reserved_encoding.extend_from_slice(&[byte, 0]);
        }
        reserved_encoding.resize(MAX_MANAGED_SNAPSHOT_PRIMARY_KEY_BYTES, 0);

        let position = ManagedSnapshotPosition::new(1, reserved_encoding).unwrap();
        assert_eq!(
            position.primary_key().len(),
            MAX_MANAGED_SNAPSHOT_PRIMARY_KEY_BYTES
        );
        assert_eq!(
            position.as_bytes().len(),
            MAX_MANAGED_SNAPSHOT_POSITION_BYTES
        );
    }

    #[test]
    fn managed_text_types_deserialize_borrowed_bounded_strings() {
        let database = database_id(1);
        let database_text = database.to_string();
        assert_eq!(
            DatabaseId::deserialize(BorrowedStrOnly(&database_text)).unwrap(),
            database
        );
        let storage = storage_id(2);
        let storage_text = storage.to_string();
        assert_eq!(
            StorageId::deserialize(BorrowedStrOnly(&storage_text)).unwrap(),
            storage
        );

        let digest_hex = "ab".repeat(DIGEST_BYTES);
        assert_eq!(
            ManagedSnapshotDigest::deserialize(BorrowedStrOnly(&digest_hex))
                .unwrap()
                .to_string(),
            digest_hex
        );

        let position_hex = "0001abcd";
        assert_eq!(
            ManagedSnapshotPosition::deserialize(BorrowedStrOnly(position_hex))
                .unwrap()
                .to_string(),
            position_hex
        );

        let oversized_ulid = "0".repeat(ulid::ULID_LEN + 1);
        assert!(DatabaseId::deserialize(BorrowedStrOnly(&oversized_ulid)).is_err());
        assert!(StorageId::deserialize(BorrowedStrOnly(&oversized_ulid)).is_err());
        let oversized_digest = "a".repeat(DIGEST_HEX_BYTES + 2);
        assert!(ManagedSnapshotDigest::deserialize(BorrowedStrOnly(&oversized_digest)).is_err());
        let oversized_position = "00".repeat(MAX_MANAGED_SNAPSHOT_POSITION_BYTES + 1);
        assert!(
            ManagedSnapshotPosition::deserialize(BorrowedStrOnly(&oversized_position)).is_err()
        );
    }

    #[test]
    fn page_limit_rejects_zero_oversize_and_serde_bypass() {
        assert!(ManagedSnapshotPageLimit::new(0).is_err());
        assert!(ManagedSnapshotPageLimit::new(MAX_MANAGED_SNAPSHOT_PAGE_RECORDS + 1).is_err());
        assert!(serde_json::from_str::<ManagedSnapshotPageLimit>("0").is_err());
        assert!(
            serde_json::from_str::<ManagedSnapshotPageLimit>(
                &(MAX_MANAGED_SNAPSHOT_PAGE_RECORDS + 1).to_string()
            )
            .is_err()
        );
    }

    #[test]
    fn pages_reject_reordering_duplicates_and_request_bound_violations() {
        let head = managed_head(7);
        let page_request = request(head, None, 2);
        assert_eq!(
            ManagedSnapshotPage::new(&page_request, vec![record(2), record(1)], false),
            Err(ManagedDomainError::NonCanonicalSnapshotOrder)
        );
        assert_eq!(
            ManagedSnapshotPage::new(&page_request, vec![record(1), record(1)], false),
            Err(ManagedDomainError::DuplicateSnapshotPosition {
                position: position(1)
            })
        );
        assert_eq!(
            ManagedSnapshotPage::new(&page_request, vec![record(1), record(2), record(3)], false),
            Err(ManagedDomainError::PageExceedsRequest {
                actual: 3,
                limit: 2,
            })
        );
        assert_eq!(
            ManagedSnapshotPage::new(&page_request, Vec::new(), true),
            Err(ManagedDomainError::EmptyContinuation)
        );

        let relation_order = vec![
            ManagedSnapshotRecordCommitment::new(
                ManagedSnapshotPosition::new(2, [0]).unwrap(),
                digest(1),
            ),
            ManagedSnapshotRecordCommitment::new(
                ManagedSnapshotPosition::new(1, [255]).unwrap(),
                digest(2),
            ),
        ];
        assert_eq!(
            ManagedSnapshotPage::new(&page_request, relation_order, false),
            Err(ManagedDomainError::NonCanonicalSnapshotOrder)
        );
    }

    #[test]
    fn cursor_continuation_binds_head_and_exclusive_position() {
        let head = managed_head(7);
        let first_request = request(head, None, 2);
        let first =
            ManagedSnapshotPage::new(&first_request, vec![record(1), record(2)], true).unwrap();
        let cursor = first.next().unwrap().clone();
        assert_eq!(cursor.after(), &position(2));

        let second_request = request(head, Some(cursor.clone()), 2);
        let second = ManagedSnapshotPage::new(&second_request, vec![record(3)], false).unwrap();
        assert!(second.next().is_none());
        assert_ne!(second.page_digest(), first.page_digest());
        assert!(second.validate_against(&second_request).is_ok());

        assert_eq!(
            ManagedSnapshotRequest::new(
                managed_head(8),
                Some(cursor.clone()),
                ManagedSnapshotPageLimit::new(2).unwrap(),
            ),
            Err(ManagedDomainError::CursorHeadMismatch)
        );
        assert_eq!(
            ManagedSnapshotPage::new(&second_request, vec![record(2)], false),
            Err(ManagedDomainError::DuplicateSnapshotPosition {
                position: position(2)
            })
        );
    }

    #[test]
    fn cursor_serde_rejects_invalid_or_unknown_positions() {
        let head = managed_head(1);
        let cursor = ManagedSnapshotCursor::new(head.cursor_binding(), position(1));

        let mut value = serde_json::to_value(cursor).unwrap();
        value["after"] = json!("0001");
        assert!(serde_json::from_value::<ManagedSnapshotCursor>(value).is_err());

        let cursor = ManagedSnapshotCursor::new(head.cursor_binding(), position(1));
        let mut value = serde_json::to_value(cursor).unwrap();
        value["authority"] = json!("lol no");
        assert!(serde_json::from_value::<ManagedSnapshotCursor>(value).is_err());
    }

    #[test]
    fn empty_terminal_page_is_valid_but_claims_no_complete_digest() {
        let head = ManagedStoreHead::unmanaged(database_id(9));
        let page = ManagedSnapshotPage::new(&request(head, None, 16), Vec::new(), false).unwrap();
        assert!(page.next().is_none());
        assert!(!page.has_more());
        assert_eq!(page.records(), []);
    }

    #[test]
    fn page_deserialization_recomputes_all_derived_commitments() {
        let head = managed_head(3);
        let page =
            ManagedSnapshotPage::new(&request(head, None, 2), vec![record(1)], true).unwrap();
        let encoded = serde_json::to_string(&page).unwrap();
        let round_trip: ManagedSnapshotPage = serde_json::from_str(&encoded).unwrap();
        assert_eq!(round_trip, page);

        let mut forged: Value = serde_json::from_str(&encoded).unwrap();
        forged["page_digest"] = json!(digest(99).to_string());
        assert!(serde_json::from_value::<ManagedSnapshotPage>(forged).is_err());

        let mut forged: Value = serde_json::from_str(&encoded).unwrap();
        forged["next"]["after"] = json!(position(2).to_string());
        assert!(serde_json::from_value::<ManagedSnapshotPage>(forged).is_err());

        let mut forged: Value = serde_json::from_str(&encoded).unwrap();
        forged["records"].as_array_mut().unwrap().push(json!({
            "position": position(0).to_string(),
            "digest": digest(1).to_string()
        }));
        assert!(serde_json::from_value::<ManagedSnapshotPage>(forged).is_err());

        let mut forged: Value = serde_json::from_str(&encoded).unwrap();
        forged["records"] = Value::Array(
            (0..=MAX_MANAGED_SNAPSHOT_PAGE_RECORDS)
                .map(|index| {
                    json!({
                        "position": ManagedSnapshotPosition::new(
                            1,
                            [u8::try_from(index % 256).unwrap()]
                        ).unwrap().to_string(),
                        "digest": digest(1).to_string()
                    })
                })
                .collect(),
        );
        assert!(serde_json::from_value::<ManagedSnapshotPage>(forged).is_err());
    }

    #[test]
    fn cursor_binding_changes_for_every_managed_head_component() {
        let base = managed_head(1);
        let variants = [
            ManagedStoreHead::managed(
                database_id(10),
                base.storage().unwrap(),
                base.managed_schema_version().unwrap(),
                base.minimum_writer_schema().unwrap(),
                base.mutation_epoch().unwrap(),
            ),
            ManagedStoreHead::managed(
                base.database_id(),
                StorageIdentity::new(storage_id(11), base.storage().unwrap().generation()),
                base.managed_schema_version().unwrap(),
                base.minimum_writer_schema().unwrap(),
                base.mutation_epoch().unwrap(),
            ),
            ManagedStoreHead::managed(
                base.database_id(),
                StorageIdentity::new(
                    base.storage().unwrap().id(),
                    StorageGeneration::new(12).unwrap(),
                ),
                base.managed_schema_version().unwrap(),
                base.minimum_writer_schema().unwrap(),
                base.mutation_epoch().unwrap(),
            ),
            ManagedStoreHead::managed(
                base.database_id(),
                base.storage().unwrap(),
                ManagedSchemaVersion::new(5).unwrap(),
                base.minimum_writer_schema().unwrap(),
                base.mutation_epoch().unwrap(),
            ),
            ManagedStoreHead::managed(
                base.database_id(),
                base.storage().unwrap(),
                base.managed_schema_version().unwrap(),
                WriterSchemaVersion::new(21).unwrap(),
                base.mutation_epoch().unwrap(),
            ),
            ManagedStoreHead::managed(
                base.database_id(),
                base.storage().unwrap(),
                base.managed_schema_version().unwrap(),
                base.minimum_writer_schema().unwrap(),
                MutationEpoch::new(2).unwrap(),
            ),
        ];
        for variant in variants {
            assert_ne!(variant.cursor_binding(), base.cursor_binding());
        }
        assert_ne!(
            ManagedStoreHead::unmanaged(base.database_id()).cursor_binding(),
            base.cursor_binding()
        );
    }
}

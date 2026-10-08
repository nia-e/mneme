//! Immutable authored annotations over summary-only historical evidence.
//!
//! Reference resolution belongs to the native atomic capture commit. The
//! engine never preloads target nodes to manufacture snapshots, and replay
//! never rechecks today's targets against yesterday's authored evidence.

use mneme_core::touchstone::{
    SummarySnapshot, TouchstoneInput, TouchstonePage, TouchstonePageRequest, TouchstoneRecord,
    TouchstoneReferrersRequest, TouchstoneStore,
};
use sha2::{Digest, Sha256};

use super::{Error, Memory, NodeId, Result};

/// Explicit successor codec: the unchanged capture-v2 digest binds ordinary
/// authored fields, and the canonical typed bytes bind subject/references.
/// Neither representation depends on Debug output or target mutable state.
pub(crate) fn request_digest(base_capture_v2: [u8; 32], input: &TouchstoneInput) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"mneme.capture.request.touchstone.v1\0");
    hash.update(base_capture_v2);
    hash.update(input.canonical_bytes());
    hash.finalize().into()
}

impl Memory {
    pub(crate) fn touchstone_store(&self) -> Result<&dyn TouchstoneStore> {
        self.graph.touchstones().ok_or_else(|| {
            Error::InvalidInput("native touchstone operations are unsupported by this store".into())
        })
    }

    /// Full immutable annotation metadata, including original summary snapshots.
    /// A bare tag does not become a typed record. No body is resolved.
    pub async fn get_touchstone(&self, owner: NodeId) -> Result<Option<TouchstoneRecord>> {
        self.touchstone_store()?.get_touchstone(owner).await
    }

    /// A native summary-only binding for authoring and current-status readback.
    /// Equality makes no assertion about bodies, tags, lifecycle or episode head.
    pub async fn summary_snapshot(&self, id: NodeId) -> Result<Option<SummarySnapshot>> {
        self.touchstone_store()?.summary_snapshot(id).await
    }

    /// Indexed owner collection with a scope-bound keyset continuation.
    pub async fn touchstones_page(
        &self,
        request: &TouchstonePageRequest,
    ) -> Result<TouchstonePage> {
        let store = self.touchstone_store()?;
        request.validate(store.database_id()?)?;
        store.touchstones_page(request).await
    }

    /// Indexed one-hop incoming annotations. Callers keep discovery bounded;
    /// this method never recursively walks references or calls a model.
    pub async fn touchstone_referrers_page(
        &self,
        request: &TouchstoneReferrersRequest,
    ) -> Result<TouchstonePage> {
        let store = self.touchstone_store()?;
        request.validate(store.database_id()?)?;
        store.touchstone_referrers_page(request).await
    }
}

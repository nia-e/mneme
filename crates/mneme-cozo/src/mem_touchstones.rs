//! Summary-only authored snapshots. Target changes never update these records.
use super::*;

pub(crate) fn validate_pre_touchstone_export(export: &StoreExport) -> Result<()> {
    if !export.touchstones.is_empty()
        || export.nodes.iter().any(|node| {
            matches!(node.provenance(), Provenance::External { source }
            if source.request_codec() == mneme_core::CaptureRequestCodec::TouchstoneV1)
        })
    {
        return Err(Error::InvalidInput(
            "predecessor export cannot contain touchstones or touchstone_v1 codec".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_touchstone_import(export: &StoreExport) -> Result<()> {
    let nodes = export
        .nodes
        .iter()
        .map(|node| (node.id(), node))
        .collect::<BTreeMap<_, _>>();
    let vectors = export
        .vectors
        .iter()
        .map(|(id, _)| *id)
        .collect::<BTreeSet<_>>();
    let mut owners = BTreeSet::new();
    for record in &export.touchstones {
        if !owners.insert(record.owner()) {
            return Err(Error::InvalidInput("duplicate touchstone owner".into()));
        }
        let node = nodes
            .get(&record.owner())
            .ok_or_else(|| Error::InvalidInput("touchstone has missing owner".into()))?;
        record.validate_owner(export.db_id, node)?;
        if !vectors.contains(&record.owner()) {
            return Err(Error::InvalidInput(
                "touchstone owner has no searchable vector projection".into(),
            ));
        }
    }
    for node in &export.nodes {
        if matches!(node.provenance(), Provenance::External {source}
            if source.request_codec() == mneme_core::CaptureRequestCodec::TouchstoneV1)
            && !owners.contains(&node.id())
        {
            return Err(Error::InvalidInput(
                "touchstone capture owner is missing immutable record".into(),
            ));
        }
    }
    Ok(())
}

/// Called after replay admission and before any writes, under the backend's
/// canonical writer lock. No target body, tags or current episode head is read.
pub(crate) fn prepare_touchstone(
    db_id: Ulid,
    node: &Node,
    input: Option<&TouchstoneInput>,
    mut get: impl FnMut(NodeId) -> Result<Option<Node>>,
) -> Result<Option<TouchstoneRecord>> {
    let Some(input) = input else {
        if matches!(node.provenance(), Provenance::External {source}
            if source.request_codec() == mneme_core::CaptureRequestCodec::TouchstoneV1)
        {
            return Err(Error::InvalidInput(
                "touchstone codec requires immutable record".into(),
            ));
        }
        return Ok(None);
    };
    input.validate()?;
    // Scope refusal precedes all target reads (and all mutations).
    for reference in input.references() {
        if reference.db_id() != db_id {
            return Err(Error::InvalidInput(
                "touchstone references must name this logical database".into(),
            ));
        }
        if reference.id() == node.id() {
            return Err(Error::InvalidInput(
                "touchstone cannot refer to its own new owner".into(),
            ));
        }
    }
    let mut snapshots = Vec::with_capacity(input.references().len());
    for reference in input.references() {
        let target = get(reference.id())?.ok_or_else(|| {
            Error::Conflict(format!(
                "touchstone target {} does not exist",
                reference.id().0
            ))
        })?;
        let snapshot = SummarySnapshot::from_node(db_id, &target)?;
        if snapshot.digest() != reference.expected_snapshot_sha256() {
            return Err(Error::Conflict(format!(
                "touchstone target {} summary snapshot changed",
                reference.id().0
            )));
        }
        snapshots.push(snapshot);
    }
    let record = TouchstoneRecord::new(node.id(), input, snapshots)?;
    record.validate_owner(db_id, node)?;
    Ok(Some(record))
}

impl Inner {
    pub(crate) fn insert_touchstone(&mut self, record: TouchstoneRecord) {
        for snapshot in record.references() {
            self.touchstone_targets
                .insert((snapshot.id(), record.owner()));
        }
        self.touchstones.insert(record.owner(), record);
    }
    pub(crate) fn remove_touchstone(&mut self, owner: NodeId) {
        if let Some(record) = self.touchstones.remove(&owner) {
            for snapshot in record.references() {
                self.touchstone_targets.remove(&(snapshot.id(), owner));
            }
        }
    }
    pub(crate) fn verify_touchstone_replay(
        &self,
        db_id: Ulid,
        dim: usize,
        node: &Node,
        input: Option<&TouchstoneInput>,
    ) -> Result<()> {
        let has_codec = matches!(node.provenance(), Provenance::External {source}
            if source.request_codec() == mneme_core::CaptureRequestCodec::TouchstoneV1);
        match self.touchstones.get(&node.id()) {
            Some(record) => {
                record.validate_owner(db_id, node)?;
                let vector = self.vectors.get(&node.id()).ok_or_else(|| {
                    Error::Conflict("touchstone replay has no vector projection".into())
                })?;
                if !self
                    .tag_projection
                    .indexes_node_exactly(node, (stable_tag_sample_hash(node.id()), node.id()))
                {
                    return Err(Error::Conflict(
                        "touchstone replay has inconsistent tag projection".into(),
                    ));
                }
                if vector.len() != dim {
                    return Err(Error::Conflict(
                        "touchstone replay has wrong vector dimension".into(),
                    ));
                }
                validate_cosine_vector(vector, "touchstone replay vector").map_err(|error| {
                    Error::Conflict(format!("touchstone replay has invalid vector: {error}"))
                })?;
                if input.is_some_and(|input| {
                    record.subject() != input.subject()
                        || record.references().len() != input.references().len()
                        || record.references().iter().zip(input.references()).any(
                            |(snapshot, reference)| {
                                snapshot.id() != reference.id()
                                    || snapshot.db_id() != reference.db_id()
                                    || snapshot.digest() != reference.expected_snapshot_sha256()
                            },
                        )
                }) {
                    return Err(Error::Conflict(
                        "touchstone replay differs from committed record".into(),
                    ));
                }
                for snapshot in record.references() {
                    if !self
                        .touchstone_targets
                        .contains(&(snapshot.id(), node.id()))
                    {
                        return Err(Error::Conflict(
                            "touchstone replay has incomplete reverse index".into(),
                        ));
                    }
                }
                Ok(())
            }
            None if has_codec || input.is_some() => Err(Error::Conflict(
                "touchstone replay has no immutable record".into(),
            )),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl TouchstoneStore for MemStore {
    fn database_id(&self) -> Result<Ulid> {
        Ok(self.db_id)
    }
    async fn summary_snapshot(&self, id: NodeId) -> Result<Option<SummarySnapshot>> {
        self.lock()
            .nodes
            .get(&id)
            .map(|node| SummarySnapshot::from_node(self.db_id, node))
            .transpose()
    }
    async fn get_touchstone(&self, owner: NodeId) -> Result<Option<TouchstoneRecord>> {
        let g = self.lock();
        let Some(record) = g.touchstones.get(&owner) else {
            return Ok(None);
        };
        record.validate_owner(self.db_id, g.nodes.get(&owner).ok_or(Error::NotFound)?)?;
        Ok(Some(record.clone()))
    }
    async fn touchstones_page(&self, request: &TouchstonePageRequest) -> Result<TouchstonePage> {
        request.validate(self.db_id)?;
        use std::ops::Bound::{Excluded, Unbounded};
        let g = self.lock();
        let lower = request
            .after()
            .map_or(Unbounded, |cursor| Excluded(cursor.after()));
        let rows = g
            .touchstones
            .range((lower, Unbounded))
            .take(request.limit())
            .collect::<Vec<_>>();
        let mut items = Vec::with_capacity(rows.len());
        for &(id, record) in &rows {
            if request
                .subject()
                .is_none_or(|subject| subject == record.subject())
            {
                items.push(TouchstoneHeader::from_node(
                    g.nodes.get(id).ok_or(Error::NotFound)?,
                    record,
                )?);
            }
        }
        // A full bounded page does not prove exhaustion. Return an empty final
        // page if necessary rather than performing an unbudgeted lookahead.
        let next = if rows.len() == request.limit() {
            rows.last().map(|(id, _)| {
                TouchstoneCursor::owners(self.db_id, request.subject().cloned(), **id)
            })
        } else {
            None
        };
        Ok(TouchstonePage { items, next })
    }
    async fn touchstone_referrers_page(
        &self,
        request: &TouchstoneReferrersRequest,
    ) -> Result<TouchstonePage> {
        request.validate(self.db_id)?;
        use std::ops::Bound::{Excluded, Included};
        let g = self.lock();
        let target = request.target();
        let lower = request
            .after()
            .map_or(Included((target, NodeId(Ulid::nil()))), |cursor| {
                Excluded((target, cursor.after()))
            });
        let rows = g
            .touchstone_targets
            .range((lower, Included((target, NodeId(Ulid::from(u128::MAX))))))
            .take(request.limit())
            .copied()
            .collect::<Vec<_>>();
        let mut items = Vec::with_capacity(rows.len());
        for (_, owner) in &rows {
            let record = g
                .touchstones
                .get(owner)
                .ok_or_else(|| Error::Backend("orphan touchstone reverse row".into()))?;
            items.push(TouchstoneHeader::from_node(
                g.nodes.get(owner).ok_or(Error::NotFound)?,
                record,
            )?);
        }
        let next = if rows.len() == request.limit() {
            rows.last()
                .map(|(_, owner)| TouchstoneCursor::referrers(self.db_id, target, *owner))
        } else {
            None
        };
        Ok(TouchstonePage { items, next })
    }
}

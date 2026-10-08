//! Immutable summary-only owner records and an indexed target-to-owner view.
use super::*;

pub(super) fn tx_generation(tx: &MultiTransaction) -> Result<bool> {
    let rows = tx_run(
        tx,
        "?[v] := *meta{k: $key, v}",
        BTreeMap::from([("key".into(), dv_str(VECTOR_PROJECTION_META_KEY))]),
    )?;
    Ok(
        matches!(rows.rows.as_slice(), [row] if want_str(&row[0])? == TOUCHSTONES_V1_CATALOG_GENERATION_MARKER),
    )
}
fn tx_database_id(tx: &MultiTransaction) -> Result<Ulid> {
    let rows = tx_run(tx, "?[v] := *meta{k: 'db_id', v}", BTreeMap::new())?;
    let [row] = rows.rows.as_slice() else {
        return Err(backend_str("missing logical database identity".into()));
    };
    want_str(&row[0])?
        .parse()
        .map_err(|_| backend_str("invalid logical database identity".into()))
}
fn decode_record(owner: NodeId, data: &str) -> Result<TouchstoneRecord> {
    if data.len() > MAX_TOUCHSTONE_RECORD_BYTES {
        return Err(backend_str(
            "touchstone record exceeds storage byte bound".into(),
        ));
    }
    let record: TouchstoneRecord = serde_json::from_str(data)
        .map_err(|e| backend_str(format!("invalid touchstone record: {e}")))?;
    if record.owner() != owner {
        return Err(backend_str(
            "touchstone row owner differs from record".into(),
        ));
    }
    Ok(record)
}
fn tx_node(tx: &MultiTransaction, id: NodeId) -> Result<Option<Node>> {
    let rows = tx_run(
        tx,
        "?[id,data,status] := *node{id,data,status}, id==$id",
        BTreeMap::from([("id".into(), dv_str(&id.0.to_string()))]),
    )?;
    rows.rows
        .first()
        .map(|row| decode_canonical_node_row(row))
        .transpose()
}
pub(super) fn tx_record(tx: &MultiTransaction, owner: NodeId) -> Result<Option<TouchstoneRecord>> {
    if !tx_generation(tx)? {
        return Ok(None);
    }
    let rows = tx_run(
        tx,
        "?[data] := *touchstone{owner:$owner,data}",
        BTreeMap::from([("owner".into(), dv_str(&owner.0.to_string()))]),
    )?;
    rows.rows
        .first()
        .map(|row| decode_record(owner, want_str(&row[0])?))
        .transpose()
}
pub(super) fn tx_prepare_record(
    tx: &MultiTransaction,
    node: &Node,
    input: Option<&TouchstoneInput>,
) -> Result<Option<TouchstoneRecord>> {
    if input.is_some() && !tx_generation(tx)? {
        return Err(Error::InvalidInput(
            "touchstone capture requires touchstones-v1 generation; explicitly upgrade this store"
                .into(),
        ));
    }
    if tx_record(tx, node.id())?.is_some() {
        return Err(Error::Conflict("orphan touchstone owner record".into()));
    }
    if tx_generation(tx)? {
        let orphan = tx_run(
            tx,
            "?[target] := *touchstone_target:by_owner{owner:$owner,target} :limit 1",
            BTreeMap::from([("owner".into(), dv_str(&node.id().0.to_string()))]),
        )?;
        if !orphan.rows.is_empty() {
            return Err(Error::Conflict("orphan touchstone reverse index".into()));
        }
    }
    let id = tx_database_id(tx)?;
    crate::mem_touchstones::prepare_touchstone(id, node, input, |target| tx_node(tx, target))
}
pub(super) fn tx_write_record(tx: &MultiTransaction, record: &TouchstoneRecord) -> Result<()> {
    let data = serde_json::to_string(record)
        .map_err(|e| backend_str(format!("encode touchstone: {e}")))?;
    if data.len() > MAX_TOUCHSTONE_RECORD_BYTES {
        return Err(Error::InvalidInput(
            "touchstone record exceeds byte bound".into(),
        ));
    }
    let mut p = BTreeMap::from([
        ("owner".into(), dv_str(&record.owner().0.to_string())),
        ("data".into(), dv_str(&data)),
    ]);
    tx_run(
        tx,
        "?[owner,data] <- [[$owner,$data]] :put touchstone {owner=>data}",
        p.clone(),
    )?;
    capture::maybe_fail_capture(record.owner(), 6)?;
    for snapshot in record.references() {
        p.insert("target".into(), dv_str(&snapshot.id().0.to_string()));
        tx_run(
            tx,
            "?[target,owner] <- [[$target,$owner]] :put touchstone_target {target,owner}",
            p.clone(),
        )?;
        capture::maybe_fail_capture(record.owner(), 7)?;
    }
    Ok(())
}
pub(super) fn tx_verify_replay(
    tx: &MultiTransaction,
    node: &Node,
    input: Option<&TouchstoneInput>,
) -> Result<()> {
    let record = tx_record(tx, node.id())?;
    let has_codec = matches!(node.provenance(), Provenance::External {source}
        if source.request_codec() == mneme_core::CaptureRequestCodec::TouchstoneV1);
    let Some(record) = record else {
        return if has_codec || input.is_some() {
            Err(Error::Conflict(
                "touchstone replay has missing immutable record".into(),
            ))
        } else {
            Ok(())
        };
    };
    record.validate_owner(tx_database_id(tx)?, node)?;
    if input.is_some_and(|input| {
        record.subject() != input.subject()
            || record.references().len() != input.references().len()
            || record
                .references()
                .iter()
                .zip(input.references())
                .any(|(s, r)| {
                    s.id() != r.id()
                        || s.db_id() != r.db_id()
                        || s.digest() != r.expected_snapshot_sha256()
                })
    }) {
        return Err(Error::Conflict(
            "touchstone replay differs from committed record".into(),
        ));
    }
    let rows = tx_run(
        tx,
        "?[target] := *touchstone_target:by_owner{owner:$owner,target} :limit $cap",
        BTreeMap::from([
            ("owner".into(), dv_str(&node.id().0.to_string())),
            ("cap".into(), dv_int((record.references().len() + 1) as i64)),
        ]),
    )?;
    let actual = rows
        .rows
        .iter()
        .map(|row| node_id(want_str(&row[0])?))
        .collect::<Result<BTreeSet<_>>>()?;
    let expected = record
        .references()
        .iter()
        .map(SummarySnapshot::id)
        .collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(Error::Conflict(
            "touchstone replay has incomplete reverse index".into(),
        ));
    }
    Ok(())
}
pub(super) fn tx_validate_owner_replacement(
    tx: &MultiTransaction,
    replacement: &Node,
) -> Result<()> {
    if tx_record(tx, replacement.id())?.is_some() {
        let before = tx_node(tx, replacement.id())?.ok_or(Error::NotFound)?;
        validate_touchstone_owner_replacement(&before, replacement)?;
    }
    Ok(())
}
pub(super) fn tx_reject_owner_merge(tx: &MultiTransaction, ids: &[NodeId]) -> Result<()> {
    for &id in ids {
        if tx_record(tx, id)?.is_some() {
            return Err(Error::Conflict("full merge cannot consume a touchstone owner; author a new interpretation and explicit supersession".into()));
        }
    }
    Ok(())
}
pub(super) fn tx_delete_owner(tx: &MultiTransaction, owner: NodeId) -> Result<()> {
    if !tx_generation(tx)? {
        return Ok(());
    }
    let p = BTreeMap::from([("owner".into(), dv_str(&owner.0.to_string()))]);
    tx_run(
        tx,
        "?[target,owner] := *touchstone_target:by_owner{owner:$owner,target}, owner=$owner :rm touchstone_target {target,owner}",
        p.clone(),
    )?;
    tx_run(
        tx,
        "?[owner] := *touchstone{owner:$owner}, owner=$owner :rm touchstone {owner}",
        p,
    )?;
    Ok(())
}
impl CozoStore {
    pub(super) fn touchstones_generation(&self) -> Result<bool> {
        Ok(self.read_meta(VECTOR_PROJECTION_META_KEY)?.as_deref()
            == Some(TOUCHSTONES_V1_CATALOG_GENERATION_MARKER))
    }
    pub(super) fn export_touchstones(&self) -> Result<Vec<TouchstoneRecord>> {
        if !self.touchstones_generation()? {
            return Ok(Vec::new());
        }
        let tx = self.db.multi_transaction(false);
        let rows = tx_run(
            &tx,
            "?[owner,data] := *touchstone{owner,data}",
            BTreeMap::new(),
        )?;
        let records = rows
            .rows
            .iter()
            .map(|row| decode_record(node_id(want_str(&row[0])?)?, want_str(&row[1])?))
            .collect::<Result<Vec<_>>>()?;
        for record in &records {
            let node = tx_node(&tx, record.owner())?.ok_or(Error::NotFound)?;
            tx_verify_replay(&tx, &node, None)?;
        }
        let reverse = tx_run(
            &tx,
            "?[target,owner] := *touchstone_target{target,owner}",
            BTreeMap::new(),
        )?;
        let expected = records
            .iter()
            .flat_map(|r| r.references().iter().map(move |s| (s.id(), r.owner())))
            .collect::<BTreeSet<_>>();
        let actual = reverse
            .rows
            .iter()
            .map(|row| Ok((node_id(want_str(&row[0])?)?, node_id(want_str(&row[1])?)?)))
            .collect::<Result<BTreeSet<_>>>()?;
        if actual != expected {
            return Err(backend_str(
                "touchstone reverse index disagrees with immutable owner records".into(),
            ));
        }
        tx.commit().map_err(backend)?;
        Ok(records)
    }
    pub(super) fn import_touchstones(&self, export: &crate::StoreExport) -> Result<()> {
        if export.touchstones.is_empty() {
            return Ok(());
        }
        let tx = self.db.multi_transaction(true);
        let staged = (|| {
            if !tx_generation(&tx)? {
                return Err(Error::InvalidInput(
                    "touchstones require touchstones-v1 generation".into(),
                ));
            }
            for record in &export.touchstones {
                tx_write_record(&tx, record)?;
            }
            Ok(())
        })();
        overlay::finish_transaction(&tx, staged)
    }
    async fn touchstone_headers(&self, owners: Vec<NodeId>) -> Result<Vec<TouchstoneHeader>> {
        self.episode_job(move |db| {
            let tx = db.multi_transaction(false);
            let mut items = Vec::with_capacity(owners.len());
            for owner in owners {
                let Some(record) = tx_record(&tx, owner)? else {
                    continue;
                };
                let node = tx_node(&tx, owner)?.ok_or(Error::NotFound)?;
                items.push(TouchstoneHeader::from_node(&node, &record)?);
            }
            tx.commit().map_err(backend)?;
            Ok(items)
        })
        .await
    }
}
#[async_trait]
impl TouchstoneStore for CozoStore {
    fn database_id(&self) -> Result<Ulid> {
        Ok(self.db_id)
    }
    async fn summary_snapshot(&self, id: NodeId) -> Result<Option<SummarySnapshot>> {
        let db_id = self.db_id;
        self.episode_job(move |db| {
            let tx = db.multi_transaction(false);
            let snapshot = tx_node(&tx, id)?
                .as_ref()
                .map(|n| SummarySnapshot::from_node(db_id, n))
                .transpose()?;
            tx.commit().map_err(backend)?;
            Ok(snapshot)
        })
        .await
    }
    async fn get_touchstone(&self, owner: NodeId) -> Result<Option<TouchstoneRecord>> {
        self.episode_job(move |db| {
            let tx = db.multi_transaction(false);
            let record = tx_record(&tx, owner)?;
            if record.is_some() {
                let node = tx_node(&tx, owner)?.ok_or(Error::NotFound)?;
                tx_verify_replay(&tx, &node, None)?;
            }
            tx.commit().map_err(backend)?;
            Ok(record)
        })
        .await
    }
    async fn touchstones_page(&self, request: &TouchstonePageRequest) -> Result<TouchstonePage> {
        request.validate(self.db_id)?;
        if !self.touchstones_generation()? {
            return Err(Error::InvalidInput(
                "touchstones-v1 generation required".into(),
            ));
        }
        let lower = request.after().map_or(PrimaryKeyScanBound::Unbounded, |c| {
            PrimaryKeyScanBound::Excluded(vec![dv_str(&c.after().0.to_string())])
        });
        let rows = self
            .scan_primary_key_async(
                "touchstone",
                PrimaryKeyScan {
                    prefix: Vec::new(),
                    lower,
                    upper: PrimaryKeyScanBound::Unbounded,
                    direction: PrimaryKeyScanDirection::Ascending,
                    limit: request.limit(),
                },
            )
            .await?;
        let owners = rows
            .rows
            .rows
            .iter()
            .map(|r| node_id(want_str(&r[0])?))
            .collect::<Result<Vec<_>>>()?;
        let next = if owners.len() == request.limit() {
            owners
                .last()
                .map(|id| TouchstoneCursor::owners(self.db_id, request.subject().cloned(), *id))
        } else {
            None
        };
        let mut items = self.touchstone_headers(owners).await?;
        items.retain(|h| {
            request
                .subject()
                .is_none_or(|subject| subject == &h.subject)
        });
        Ok(TouchstonePage { items, next })
    }
    async fn touchstone_referrers_page(
        &self,
        request: &TouchstoneReferrersRequest,
    ) -> Result<TouchstonePage> {
        request.validate(self.db_id)?;
        if !self.touchstones_generation()? {
            return Err(Error::InvalidInput(
                "touchstones-v1 generation required".into(),
            ));
        }
        let target = dv_str(&request.target().0.to_string());
        let lower = request.after().map_or(PrimaryKeyScanBound::Unbounded, |c| {
            PrimaryKeyScanBound::Excluded(vec![dv_str(&c.after().0.to_string())])
        });
        let rows = self
            .scan_primary_key_async(
                "touchstone_target",
                PrimaryKeyScan {
                    prefix: vec![target],
                    lower,
                    upper: PrimaryKeyScanBound::Unbounded,
                    direction: PrimaryKeyScanDirection::Ascending,
                    limit: request.limit(),
                },
            )
            .await?;
        let owners = rows
            .rows
            .rows
            .iter()
            .map(|r| node_id(want_str(&r[1])?))
            .collect::<Result<Vec<_>>>()?;
        let next = if owners.len() == request.limit() {
            owners
                .last()
                .map(|id| TouchstoneCursor::referrers(self.db_id, request.target(), *id))
        } else {
            None
        };
        let items = self.touchstone_headers(owners).await?;
        Ok(TouchstonePage { items, next })
    }
}

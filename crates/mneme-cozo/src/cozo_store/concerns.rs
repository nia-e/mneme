//! Durable advisory rows. Canonical-node meanings and exact row CAS share one transaction.
use super::*;
use mneme_core::concern::*;

fn kind_text(kind: ConcernKind) -> &'static str {
    match kind {
        ConcernKind::Disagreement => "disagreement",
        ConcernKind::Redundancy => "redundancy",
    }
}
fn decode_kind(value: &DataValue) -> Result<ConcernKind> {
    match want_str(value)? {
        "disagreement" => Ok(ConcernKind::Disagreement),
        "redundancy" => Ok(ConcernKind::Redundancy),
        _ => Err(backend_str("invalid stored concern kind".into())),
    }
}
fn params(key: ConcernKey) -> BTreeMap<String, DataValue> {
    let [lo, hi] = key.endpoints();
    BTreeMap::from([
        ("lo".into(), dv_str(&lo.0.to_string())),
        ("hi".into(), dv_str(&hi.0.to_string())),
        ("kind".into(), dv_str(kind_text(key.kind()))),
    ])
}
fn decode_row(row: &[DataValue]) -> Result<ConcernRow> {
    if row.len() != 4 {
        return Err(backend_str("malformed stored concern row".into()));
    }
    let lo = graph_records::decode_canonical_node_id(&row[0], "concern lo")?;
    let hi = graph_records::decode_canonical_node_id(&row[1], "concern hi")?;
    if lo >= hi {
        return Err(backend_str("noncanonical concern endpoints".into()));
    }
    let key =
        ConcernKey::new(decode_kind(&row[2])?, lo, hi).map_err(|e| backend_str(e.to_string()))?;
    let data = want_str(&row[3])?;
    if data.len() > crate::MAX_CONCERN_ROW_JSON_BYTES {
        return Err(backend_str("stored concern exceeds byte ceiling".into()));
    }
    let concern: ConcernRow = serde_json::from_str(data)
        .map_err(|e| backend_str(format!("invalid stored concern: {e}")))?;
    if concern.binding().key() != key {
        return Err(backend_str("concern columns disagree with row".into()));
    }
    Ok(concern)
}
fn load(tx: &MultiTransaction, key: ConcernKey) -> Result<Option<ConcernRow>> {
    let rows = tx_run(
        tx,
        "?[lo,hi,kind,data] := lo=$lo, hi=$hi, kind=$kind, *concern{lo:$lo,hi:$hi,kind:$kind,data}",
        params(key),
    )?;
    if rows.rows.len() > 1 {
        return Err(backend_str("duplicate exact concern".into()));
    }
    rows.rows.first().map(|row| decode_row(row)).transpose()
}
fn require_generation(tx: &MultiTransaction) -> Result<()> {
    let p = BTreeMap::from([("key".into(), dv_str(VECTOR_PROJECTION_META_KEY))]);
    let rows = tx_run(tx, "?[v] := *meta{k:$key,v}", p)?;
    if rows.rows.len() != 1
        || !matches!(
            want_str(&rows.rows[0][0])?,
            CONCERN_V1_CATALOG_GENERATION_MARKER
                | EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER
                | TOUCHSTONES_V1_CATALOG_GENERATION_MARKER
        )
    {
        return Err(Error::InvalidInput(
            "concern lane requires current concern generation".into(),
        ));
    }
    Ok(())
}
fn put(tx: &MultiTransaction, row: &ConcernRow) -> Result<()> {
    let mut p = params(row.binding().key());
    p.insert("data".into(), dv_str(&crate::encode_concern_row(row)?));
    tx_run(
        tx,
        "?[lo,hi,kind,data] <- [[$lo,$hi,$kind,$data]] :put concern {lo,hi,kind => data}",
        p,
    )?;
    Ok(())
}
fn stage_update(tx: &MultiTransaction, update: &ConcernUpdate) -> Result<ConcernCommitOutcome> {
    require_generation(tx)?;
    let key = update.key();
    let row = load(tx, key)?;
    let [lo, hi] = key.endpoints();
    let mut endpoints = Vec::with_capacity(2);
    for id in [lo, hi] {
        let p = BTreeMap::from([("id".into(), dv_str(&id.0.to_string()))]);
        let rows = tx_run(
            tx,
            "?[id,data,status] := id=$id, *node{id:$id,data,status}",
            p,
        )?;
        let Some(raw) = rows.rows.first() else {
            return Ok(ConcernCommitOutcome::Refused {
                reason: ConcernRefusal::MissingEndpoint,
                row,
            });
        };
        let node = decode_canonical_node_row(raw)?;
        if node.status() != NodeStatus::Active {
            return Ok(ConcernCommitOutcome::Refused {
                reason: ConcernRefusal::InactiveEndpoint,
                row,
            });
        }
        endpoints.push(ConcernEndpoint::from_node(&node));
    }
    let binding = ConcernBinding::new(key.kind(), endpoints[0], endpoints[1])
        .map_err(|e| Error::InvalidInput(e.to_string()))?;
    match transition_concern(&binding, row.as_ref(), update) {
        ConcernTransition::Unchanged => Ok(ConcernCommitOutcome::Unchanged {
            row: row.expect("unchanged row exists"),
        }),
        ConcernTransition::Refused(reason) => Ok(ConcernCommitOutcome::Refused { reason, row }),
        ConcernTransition::Replace(row) => {
            put(tx, &row)?;
            Ok(ConcernCommitOutcome::Applied { row })
        }
    }
}
fn update_sync(db: &DbInstance, update: &ConcernUpdate) -> Result<ConcernCommitOutcome> {
    let mut delay = std::time::Duration::from_millis(LOCK_RETRY_BASE_MS);
    for attempt in 0..=LOCK_RETRY_MAX {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let tx = db.multi_transaction(true);
            match stage_update(&tx, update) {
                Ok(outcome) => tx.commit().map_err(backend).map(|()| outcome),
                Err(error) => {
                    let _ = tx.abort();
                    Err(error)
                }
            }
        }));
        match result {
            Ok(Ok(outcome)) => return Ok(outcome),
            Ok(Err(error)) if is_locked(&error) && attempt < LOCK_RETRY_MAX => {}
            Ok(Err(error)) => return Err(error),
            Err(panic) if panic_is_locked(panic.as_ref()) && attempt < LOCK_RETRY_MAX => {}
            Err(panic) if panic_is_locked(panic.as_ref()) => {
                return Err(backend_str(
                    "database is locked: concern contention exhausted".into(),
                ));
            }
            Err(panic) => std::panic::resume_unwind(panic),
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(std::time::Duration::from_millis(LOCK_RETRY_CAP_MS));
    }
    unreachable!("last attempt returns")
}

impl CozoStore {
    pub(super) fn export_concerns(&self) -> Result<Vec<ConcernRow>> {
        self.run(
            "?[lo,hi,kind,data] := *concern{lo,hi,kind,data}",
            BTreeMap::new(),
            false,
        )?
        .rows
        .iter()
        .map(|row| decode_row(row))
        .collect()
    }
    pub(super) fn import_concern(&self, row: &ConcernRow) -> Result<()> {
        let tx = self.db.multi_transaction(true);
        let staged = (|| {
            require_generation(&tx)?;
            put(&tx, row)
        })();
        overlay::finish_transaction(&tx, staged)
    }
}

#[async_trait]
impl ConcernStore for CozoStore {
    async fn get_concern(&self, key: &ConcernKey) -> Result<Option<ConcernRow>> {
        let key = *key;
        let job = ActiveBackendJob {
            db: self.db.clone(),
            authority: self.persistent_authority.clone(),
            activity: self.backend_activity.enter(),
        };
        tokio::task::spawn_blocking(move || {
            job.run(|db| {
                let tx = db.multi_transaction(false);
                require_generation(&tx)?;
                let row = load(&tx, key)?;
                tx.commit().map_err(backend)?;
                Ok(row)
            })
        })
        .await
        .map_err(|e| backend_str(format!("join concern read: {e}")))?
    }
    async fn update_concern(&self, update: &ConcernUpdate) -> Result<ConcernCommitOutcome> {
        let update = update.clone();
        let job = ActiveBackendJob {
            db: self.db.clone(),
            authority: self.persistent_authority.clone(),
            activity: self.backend_activity.enter(),
        };
        tokio::task::spawn_blocking(move || job.run(|db| update_sync(db, &update)))
            .await
            .map_err(|e| backend_str(format!("join concern update: {e}")))?
    }
    async fn concerns_for_endpoint(&self, request: &ConcernPageRequest) -> Result<ConcernPage> {
        let request = request.clone();
        let job = ActiveBackendJob {
            db: self.db.clone(),
            authority: self.persistent_authority.clone(),
            activity: self.backend_activity.enter(),
        };
        tokio::task::spawn_blocking(move || {
            job.run(|db| {
                let tx = db.multi_transaction(false);
                require_generation(&tx)?;
                let endpoint = request.endpoint();
                let prefix = dv_str(&endpoint.0.to_string());
                let lower = request
                    .after()
                    .map_or(PrimaryKeyScanBound::Unbounded, |after| {
                        // Native bounds are suffix keys, relative to the fixed prefix.
                        PrimaryKeyScanBound::Excluded(vec![
                            dv_str(&after.other().0.to_string()),
                            dv_str(kind_text(after.kind())),
                        ])
                    });
                let mut keys = Vec::new();
                // Two physical prefix seeks, <=2*(limit+1) keys, then <=limit canonical rows.
                for (relation, incoming) in [("concern", false), ("concern:by_hi", true)] {
                    let rows = tx
                        .scan_relation_by_primary_key(
                            relation,
                            PrimaryKeyScan {
                                prefix: vec![prefix.clone()],
                                lower: lower.clone(),
                                upper: PrimaryKeyScanBound::Unbounded,
                                direction: PrimaryKeyScanDirection::Ascending,
                                limit: request.limit() + 1,
                            },
                        )
                        .map_err(backend)?;
                    if rows.rows.len() > request.limit() + 1 {
                        return Err(backend_str("concern page exceeded scan allowance".into()));
                    }
                    for raw in rows.rows {
                        if raw.len() < 3 {
                            return Err(backend_str("truncated concern key".into()));
                        }
                        let fixed = graph_records::decode_canonical_node_id(
                            &raw[0],
                            "concern page endpoint",
                        )?;
                        let other =
                            graph_records::decode_canonical_node_id(&raw[1], "concern page other")?;
                        let kind = decode_kind(&raw[2])?;
                        if fixed != endpoint || other == endpoint {
                            return Err(backend_str("invalid concern prefix row".into()));
                        }
                        let key = ConcernKey::new(
                            kind,
                            if incoming { other } else { fixed },
                            if incoming { fixed } else { other },
                        )
                        .map_err(|e| backend_str(e.to_string()))?;
                        if key.endpoints()
                            != if incoming {
                                [other, fixed]
                            } else {
                                [fixed, other]
                            }
                        {
                            return Err(backend_str("noncanonical concern page key".into()));
                        }
                        keys.push((other, kind, key));
                    }
                }
                keys.sort_by_key(|(other, kind, _)| (*other, *kind));
                let more = keys.len() > request.limit();
                keys.truncate(request.limit());
                let next = if more {
                    keys.last().map(|(other, kind, _)| {
                        ConcernPageCursor::new(endpoint, *other, *kind).expect("distinct endpoints")
                    })
                } else {
                    None
                };
                let mut items = Vec::with_capacity(keys.len());
                for (_, _, key) in keys {
                    items.push(load(&tx, key)?.ok_or_else(|| {
                        backend_str("concern index row missing canonical row".into())
                    })?);
                }
                tx.commit().map_err(backend)?;
                Ok(ConcernPage { items, next })
            })
        })
        .await
        .map_err(|e| backend_str(format!("join concern page: {e}")))?
    }
}

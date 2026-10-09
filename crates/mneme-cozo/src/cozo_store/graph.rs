use super::*;
use crate::storage_contract::conventional_unmanaged::spec::SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER;
use mneme_core::Provenance;
use mneme_core::ports::{IncidentEdgeLeg, IncidentEdgesPage, IncidentEdgesRequest};

#[cfg(test)]
fn summary_edit_failure_hook() -> &'static std::sync::Mutex<BTreeMap<NodeId, usize>> {
    static HOOK: std::sync::OnceLock<std::sync::Mutex<BTreeMap<NodeId, usize>>> =
        std::sync::OnceLock::new();
    HOOK.get_or_init(Default::default)
}

#[cfg(test)]
fn maybe_fail_summary_edit(id: NodeId, step: usize) -> Result<()> {
    let mut armed = summary_edit_failure_hook().lock().unwrap();
    if armed.get(&id) == Some(&step) {
        armed.remove(&id);
        return Err(backend_str(format!(
            "injected summary edit failure after step {step}"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn maybe_fail_summary_edit(_id: NodeId, _step: usize) -> Result<()> {
    Ok(())
}

// Individual programs are executed under one MultiTransaction. Cozo's
// transaction API accepts a single program per call, not a braced block list.
pub(super) const DELETE_NODE_STATEMENTS: &[&str] = &[
    "open_pair[lo, hi] := *contradiction{lo, hi, resolution}, is_null(resolution)\n\
     open_pair[lo, hi] := *contradiction{lo, hi, resolution}, resolution == 'unresolved'\n\
     ?[lo, hi] := open_pair[lo, hi], lo == $id :rm contradiction {lo, hi}",
    "open_pair[lo, hi] := *contradiction{lo, hi, resolution}, is_null(resolution)\n\
     open_pair[lo, hi] := *contradiction{lo, hi, resolution}, resolution == 'unresolved'\n\
     ?[lo, hi] := open_pair[lo, hi], hi == $id :rm contradiction {lo, hi}",
    "?[lo, hi] := *merge_candidate{lo, hi, resolution}, is_null(resolution), lo == $id :rm merge_candidate {lo, hi}",
    "?[lo, hi] := *merge_candidate{lo, hi, resolution}, is_null(resolution), hi == $id :rm merge_candidate {lo, hi}",
    "?[tag, status, sample_hash, id] := *node_tag_v2:by_id{id, tag, status, sample_hash}, id == $id :rm node_tag_v2 {tag, status, sample_hash, id}",
    "?[id] := *node_vec{id}, id == $id :rm node_vec {id}",
    "?[id] := *node_search{id}, id == $id :rm node_search {id}",
    "?[id] := *node{id}, id == $id :rm node {id}",
];

#[cfg(test)]
pub(super) fn delete_node_script() -> String {
    DELETE_NODE_STATEMENTS
        .iter()
        .map(|statement| format!("{{{statement}}}\n"))
        .collect()
}

fn replace_tags_checked(
    store: &CozoStore,
    id: NodeId,
    expected: &mneme_core::BoundedTagSet,
    tags: &mneme_core::BoundedTagSet,
    guards: Option<&mneme_core::ports::RetagContentGuards>,
) -> Result<Node> {
    let tx = store.db.multi_transaction(true);
    let mut result = None;
    let staged = (|| {
        tx_reject_episode_nodes(&tx, &[id], "atomic tag replacement")?;
        let rows = tx_run(
            &tx,
            "?[id,data,status] := *node{id,data,status}, id==$id",
            BTreeMap::from([("id".into(), dv_str(&id.0.to_string()))]),
        )?;
        let row = rows.rows.first().ok_or(Error::NotFound)?;
        let mut replacement = decode_canonical_node_row(row)?;
        if let Some(guards) = guards {
            guards.check_target(&replacement)?;
            for guard in &guards.guard_nodes {
                let rows = tx_run(
                    &tx,
                    "?[id,data,status] := id=$id, *node{id:$id,data,status}",
                    BTreeMap::from([("id".into(), dv_str(&guard.id.0.to_string()))]),
                )?;
                let node = rows
                    .rows
                    .first()
                    .map(|row| decode_canonical_node_row(row))
                    .transpose()?;
                guard.check(node.as_ref())?;
            }
        }
        if replacement.tag_set() != expected {
            return Err(Error::Conflict(
                "node tags changed; inspect current tags before retrying".into(),
            ));
        }
        replacement.replace_tags(tags.clone());
        touchstones::tx_validate_owner_replacement(&tx, &replacement)?;
        tx_run(
            &tx,
            "?[id,data,status] <- [[$id,$data,$status]] :put node {id => data,status}",
            BTreeMap::from([
                ("id".into(), dv_str(&id.0.to_string())),
                ("data".into(), dv_str(&encode_canonical_node(&replacement)?)),
                ("status".into(), dv_str(status_str(replacement.status()))),
            ]),
        )?;
        maintenance::tx_sync_tag_projection(&tx, &[&replacement])?;
        result = Some(replacement);
        Ok(())
    })();
    overlay::finish_transaction(&tx, staged)?;
    Ok(result.expect("successful transaction produced replacement"))
}

#[async_trait]
impl GraphStore for CozoStore {
    fn touchstones(&self) -> Option<&dyn TouchstoneStore> {
        matches!(self.touchstones_generation(), Ok(true)).then_some(self)
    }
    fn concerns(&self) -> Option<&dyn mneme_core::concern::ConcernStore> {
        matches!(self.read_meta(VECTOR_PROJECTION_META_KEY), Ok(Some(marker)) if matches!(marker.as_str(), CONCERN_V1_CATALOG_GENERATION_MARKER | EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER | TOUCHSTONES_V1_CATALOG_GENERATION_MARKER)).then_some(self)
    }
    fn episodes(&self) -> Option<&dyn mneme_core::episode::EpisodeStore> {
        Some(self)
    }

    async fn compare_replace_node_summary(
        &self,
        id: NodeId,
        expected: &mneme_core::SummarySnapshotDigest,
        summary: &mneme_core::NodeSummary,
        embedding: &[f32],
    ) -> Result<Node> {
        if embedding.len() != self.dim {
            return Err(Error::DimMismatch {
                index: self.dim,
                provider: embedding.len(),
            });
        }
        crate::validate_cosine_vector(embedding, "summary edit embedding")?;
        let expected = *expected;
        let summary = summary.clone();
        let embedding = embedding.to_vec();
        let db_id = self.db_id;
        let job = ActiveBackendJob {
            db: self.db.clone(),
            authority: self.persistent_authority.clone(),
            activity: self.backend_activity.enter(),
        };
        tokio::task::spawn_blocking(move || {
            job.run(|db| {
                let tx = db.multi_transaction(true);
                let staged = (|| {
                    let mut params = BTreeMap::from([("id".into(), dv_str(&id.0.to_string()))]);
                    let rows = tx_run(&tx,
                        "?[id,data,status] := *node{id,data,status}, id==$id", params.clone())?;
                    let mut node = rows.rows.first()
                        .map(|row| decode_canonical_node_row(row)).transpose()?.ok_or(Error::NotFound)?;
                    require_semantic_node(&node, "edit_summary")?;
                    tx_reject_episode_nodes(&tx, &[id], "edit_summary")?;
                    if SummarySnapshot::from_node(db_id, &node)?.digest() != expected {
                        return Err(Error::Conflict("summary snapshot changed; GET the node again".into()));
                    }
                    if touchstones::tx_record(&tx, id)?.is_some() {
                        return Err(Error::Conflict("summary editing cannot replace a touchstone owner".into()));
                    }
                    if node.summary() == summary.as_str() {
                        return Err(Error::InvalidInput("summary is unchanged".into()));
                    }
                    node.set_summary(summary);
                    params.insert("data".into(), dv_str(&encode_canonical_node(&node)?));
                    params.insert("status".into(), dv_str(status_str(node.status())));
                    tx_run(&tx,
                        "?[id,data,status] <- [[$id,$data,$status]] :put node {id=>data,status}", params.clone())?;
                    maybe_fail_summary_edit(id, 1)?;
                    params.insert("e".into(), dv_float_list(&embedding));
                    tx_run(&tx,
                        "?[id,e,status] := id=$id, e=vec($e), status=$status :put node_vec {id=>e,status}", params.clone())?;
                    maybe_fail_summary_edit(id, 2)?;
                    params.insert("summary".into(), dv_str(node.summary()));
                    tx_run(&tx,
                        "?[id,summary,status] <- [[$id,$summary,$status]] :put node_search {id=>summary,status}", params)?;
                    maybe_fail_summary_edit(id, 3)?;
                    // Summary editing does not touch tags, status, bodies, edges
                    // or authored historical snapshots.
                    Ok(node)
                })();
                match staged {
                    Ok(node) => { tx.commit().map_err(backend)?; Ok(node) }
                    Err(error) => { let _ = tx.abort(); Err(error) }
                }
            })
        }).await.map_err(|e| backend_str(format!("join summary edit worker: {e}")))?
    }

    async fn compare_replace_node_body(
        &self,
        id: NodeId,
        expected: &mneme_core::BodyRevision,
        replacement: &mneme_core::BodyRef,
    ) -> Result<Node> {
        let expected = expected.clone();
        let replacement = replacement.clone();
        let job = ActiveBackendJob {
            db: self.db.clone(),
            authority: self.persistent_authority.clone(),
            activity: self.backend_activity.enter(),
        };
        tokio::task::spawn_blocking(move || {
            job.run(|db| {
                let tx = db.multi_transaction(true);
                let staged = (|| {
                    let mut params = BTreeMap::from([("id".into(), dv_str(&id.0.to_string()))]);
                    let rows = tx_run(
                        &tx,
                        "?[id,data,status] := *node{id,data,status}, id==$id",
                        params.clone(),
                    )?;
                    let mut node = rows
                        .rows
                        .first()
                        .map(|row| decode_canonical_node_row(row))
                        .transpose()?
                        .ok_or(Error::NotFound)?;
                    require_semantic_node(&node, "edit_body")?;
                    tx_reject_episode_nodes(&tx, &[id], "edit_body")?;
                    if node.body_revision() != expected {
                        return Err(Error::Conflict("body revision changed".into()));
                    }
                    if touchstones::tx_record(&tx, id)?.is_some() {
                        return Err(Error::Conflict(
                            "body editing cannot replace a touchstone owner".into(),
                        ));
                    }
                    let anchors = tx_run(
                        &tx,
                        "?[to] := *edge_anchor{from:$id,to} :limit 1",
                        params.clone(),
                    )?;
                    if !anchors.rows.is_empty() {
                        return Err(Error::Conflict(
                            "body editing cannot invalidate outgoing body anchors".into(),
                        ));
                    }
                    node.set_body_reference(
                        replacement.clone(),
                        mneme_core::BodyOwnership::Managed,
                    );
                    params.insert("data".into(), dv_str(&encode_canonical_node(&node)?));
                    params.insert("status".into(), dv_str(status_str(node.status())));
                    tx_run(
                        &tx,
                        "?[id,data,status] <- [[$id,$data,$status]] :put node {id=>data,status}",
                        params,
                    )?;
                    Ok(node)
                })();
                match staged {
                    Ok(node) => {
                        tx.commit().map_err(backend)?;
                        Ok(node)
                    }
                    Err(error) => {
                        let _ = tx.abort();
                        Err(error)
                    }
                }
            })
        })
        .await
        .map_err(|error| backend_str(format!("join body edit worker: {error}")))?
    }

    async fn lookup_capture(
        &self,
        proof: &mneme_core::CaptureReplayProof,
    ) -> Result<Option<NodeId>> {
        self.lookup_capture_atomic(proof).await
    }

    async fn commit_capture(
        &self,
        node: &Node,
        embedding: &[f32],
        proof: &mneme_core::CaptureReplayProof,
    ) -> Result<mneme_core::ports::CaptureCommitOutcome> {
        self.commit_capture_with_edges(node, embedding, &[], proof)
            .await
    }

    async fn commit_capture_with_edges(
        &self,
        node: &Node,
        embedding: &[f32],
        edges: &[Edge],
        proof: &mneme_core::CaptureReplayProof,
    ) -> Result<mneme_core::ports::CaptureCommitOutcome> {
        self.commit_capture_with_priors(
            node,
            embedding,
            edges,
            &[],
            mneme_core::ports::CapturePriorBudget::new(0),
            proof,
        )
        .await
    }

    async fn commit_capture_with_priors(
        &self,
        node: &Node,
        embedding: &[f32],
        edges: &[Edge],
        priors: &[mneme_core::ports::CaptureSimilarityPrior],
        generated_link_budget: mneme_core::ports::CapturePriorBudget,
        proof: &mneme_core::CaptureReplayProof,
    ) -> Result<mneme_core::ports::CaptureCommitOutcome> {
        require_semantic_node(node, "semantic capture")?;
        self.commit_capture_atomic(
            node,
            embedding,
            edges,
            priors,
            generated_link_budget,
            proof,
            None,
        )
        .await
    }

    async fn commit_capture_with_touchstone(
        &self,
        node: &Node,
        embedding: &[f32],
        edges: &[Edge],
        priors: &[mneme_core::ports::CaptureSimilarityPrior],
        generated_link_budget: mneme_core::ports::CapturePriorBudget,
        proof: &mneme_core::CaptureReplayProof,
        touchstone: Option<&TouchstoneInput>,
    ) -> Result<mneme_core::ports::CaptureCommitOutcome> {
        require_semantic_node(node, "semantic capture")?;
        self.commit_capture_atomic(
            node,
            embedding,
            edges,
            priors,
            generated_link_budget,
            proof,
            touchstone,
        )
        .await
    }

    async fn put_node(&self, node: &Node) -> Result<()> {
        self.put_node_for_import(node, false).await
    }

    async fn get_node(&self, id: NodeId) -> Result<Option<Node>> {
        Ok(self.get_nodes(&[id]).await?.pop().flatten())
    }

    async fn get_nodes(&self, ids: &[NodeId]) -> Result<Vec<Option<Node>>> {
        if ids.len() > MAX_NODE_HYDRATION_BATCH {
            return Err(Error::CapacityExceeded {
                resource: "node hydration batch",
                limit: MAX_NODE_HYDRATION_BATCH,
            });
        }
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        // Cozo relations are sets and do not preserve caller order. Query the
        // unique keys once, then reconstruct the exact positional contract in
        // Rust so duplicate and missing identities retain their input slots.
        let mut unique = ids.to_vec();
        unique.sort_unstable();
        unique.dedup();
        let (input, params) = id_input("wanted", &unique);
        let script =
            format!("{input}\n?[id, data, status] := wanted[id], *node{{id, data, status}}");
        let rows = self.run_async(script, params, false).await?;
        let mut hydrated = HashMap::with_capacity(rows.rows.len());
        for row in &rows.rows {
            let id = node_id(want_str(&row[0])?)?;
            let node = decode_canonical_node_row(row)?;
            if hydrated.insert(id, node).is_some() {
                return Err(backend_str(format!(
                    "duplicate canonical node row returned for {id:?}"
                )));
            }
        }
        Ok(ids.iter().map(|id| hydrated.get(id).cloned()).collect())
    }

    async fn get_node_statuses(&self, ids: &[NodeId]) -> Result<Vec<Option<TaggedPhysicalStatus>>> {
        if ids.len() > MAX_NODE_STATUS_BATCH {
            return Err(Error::CapacityExceeded {
                resource: "node status batch",
                limit: MAX_NODE_STATUS_BATCH,
            });
        }
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        // Cozo relations are sets. Query every distinct key once, then restore
        // the exact positional contract for duplicate and missing identities.
        let mut unique = ids.to_vec();
        unique.sort_unstable();
        unique.dedup();
        let statuses = self.statuses_for(&unique).await?;
        Ok(ids
            .iter()
            .map(|id| {
                statuses
                    .get(id)
                    .copied()
                    .map(TaggedPhysicalStatus::from_node_status)
            })
            .collect())
    }

    async fn delete_node(&self, id: NodeId) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("id".into(), dv_str(&id.0.to_string()));
        p.insert("read_cap".into(), dv_int((MAX_INCIDENT_EDGES + 1) as i64));
        // One transaction: lifecycle-filtered ANN membership cannot outlive the
        // canonical node even briefly, and unresolved reconciliation work cannot
        // survive without both endpoints. Terminal overlays and full-merge
        // commits are deliberately untouched: they are durable decisions/retry
        // proofs, not pending work. The engine's later `VectorIndex::remove`
        // remains an idempotent cleanup call.
        let tx = self.db.multi_transaction(true);
        let staged = (|| {
            tx_reject_episode_nodes(&tx, &[id], "delete_node")?;
            let rows = tx_run(
                &tx,
                "incident[other] := *edge{from: $id, to: other}\n\
                 incident[other] := *edge{to: $id, from: other}\n\
                 ?[id, data, status] := incident[id], *node{id, data, status} :limit $read_cap",
                p.clone(),
            )?;
            if rows.rows.len() > MAX_INCIDENT_EDGES {
                return Err(crate::incident_edge_capacity_error());
            }
            for row in &rows.rows {
                if !decode_canonical_node_row(row)?.is_semantic() {
                    return Err(Error::InvalidInput(
                        "cannot delete a semantic node referenced by an episode; remove the authored evidence link explicitly first".into(),
                    ));
                }
            }
            let generation = tx_run(
                &tx,
                "?[v] := *meta{k: $key, v}",
                BTreeMap::from([("key".into(), dv_str(VECTOR_PROJECTION_META_KEY))]),
            )?;
            if matches!(
                generation
                    .rows
                    .first()
                    .map(|row| want_str(&row[0]))
                    .transpose()?,
                Some(
                    CONCERN_V1_CATALOG_GENERATION_MARKER
                        | EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER
                        | TOUCHSTONES_V1_CATALOG_GENERATION_MARKER
                )
            ) {
                tx_run(
                    &tx,
                    "?[lo,hi,kind] := lo=$id, *concern{lo:$id,hi,kind} :rm concern {lo,hi,kind}",
                    p.clone(),
                )?;
                tx_run(
                    &tx,
                    "?[lo,hi,kind] := hi=$id, *concern:by_hi{hi:$id,lo,kind} :rm concern {lo,hi,kind}",
                    p.clone(),
                )?;
            }
            touchstones::tx_delete_owner(&tx, id)?;
            for statement in DELETE_NODE_STATEMENTS {
                tx_run(&tx, statement, p.clone())?;
            }
            Ok(())
        })();
        overlay::finish_transaction(&tx, staged)
    }

    async fn all_nodes(&self, _: ColdPath) -> Result<Vec<Node>> {
        let rows = self.run(
            "?[id, data, status] := *node{id, data, status}",
            BTreeMap::new(),
            false,
        )?;
        rows.rows
            .iter()
            .map(|row| decode_canonical_node_row(row))
            .collect()
    }

    async fn tag_vocabulary_page(
        &self,
        request: &mneme_core::ports::TagVocabularyRequest,
    ) -> Result<mneme_core::ports::TagVocabularyPage> {
        super::tag_vocabulary::read(&self.db, request)
    }

    async fn compare_replace_node_tags(
        &self,
        id: NodeId,
        expected: &mneme_core::BoundedTagSet,
        tags: &mneme_core::BoundedTagSet,
    ) -> Result<Node> {
        replace_tags_checked(self, id, expected, tags, None)
    }

    async fn compare_replace_node_tags_guarded(
        &self,
        id: NodeId,
        expected: &mneme_core::BoundedTagSet,
        tags: &mneme_core::BoundedTagSet,
        guards: &mneme_core::ports::RetagContentGuards,
    ) -> Result<Node> {
        guards.validate()?;
        replace_tags_checked(self, id, expected, tags, Some(guards))
    }

    async fn set_status(&self, id: NodeId, status: NodeStatus) -> Result<()> {
        self.mutate_node(id, |n| n.set_status(status)).await
    }

    async fn maintenance_node_upper_bound(&self, _: ColdPath) -> Result<Option<NodeId>> {
        let page = self
            .scan_primary_key_async(
                "node",
                PrimaryKeyScan {
                    prefix: Vec::new(),
                    lower: PrimaryKeyScanBound::Unbounded,
                    upper: PrimaryKeyScanBound::Unbounded,
                    direction: PrimaryKeyScanDirection::Descending,
                    limit: 1,
                },
            )
            .await?;
        page.rows
            .rows
            .first()
            .map(|row| node_id(want_str(&row[0])?))
            .transpose()
    }

    async fn maintenance_nodes_page(
        &self,
        _: ColdPath,
        after: Option<NodeId>,
        through: NodeId,
        limit: usize,
    ) -> Result<MaintenanceNodePage> {
        crate::validate_maintenance_page_limit(limit)?;
        if after.is_some_and(|after| after >= through) {
            return Ok(MaintenanceNodePage {
                items: Vec::new(),
                next: None,
            });
        }
        let page = self
            .scan_primary_key_async(
                "node",
                PrimaryKeyScan {
                    prefix: Vec::new(),
                    lower: after.map_or(PrimaryKeyScanBound::Unbounded, |after| {
                        PrimaryKeyScanBound::Excluded(vec![dv_str(&after.0.to_string())])
                    }),
                    upper: PrimaryKeyScanBound::Included(vec![dv_str(&through.0.to_string())]),
                    direction: PrimaryKeyScanDirection::Ascending,
                    limit: limit + 1,
                },
            )
            .await?;
        let rows = page.rows;
        let mut items = Vec::with_capacity(rows.rows.len());
        for row in &rows.rows {
            items.push(decode_canonical_node_row(row)?);
        }
        Ok(crate::finish_maintenance_node_page(items, limit))
    }

    async fn put_edge(&self, edge: &Edge) -> Result<()> {
        edge.validate().map_err(Error::InvalidInput)?;
        let last_reinforced = i64::try_from(edge.last_reinforced()).map_err(|_| {
            Error::InvalidInput("edge last_reinforced exceeds the storage integer range".into())
        })?;
        let mut p = BTreeMap::new();
        p.insert("from".into(), dv_str(&edge.from.0.to_string()));
        p.insert("to".into(), dv_str(&edge.to.0.to_string()));
        p.insert("weight".into(), dv_float(edge.weight() as f64));
        p.insert("kind".into(), dv_str(edge_kind_str(edge.kind)));
        p.insert("last_reinforced".into(), dv_int(last_reinforced));
        p.insert("trials".into(), dv_int(i64::from(edge.trials())));
        p.insert(
            "interference".into(),
            dv_int(i64::from(edge.interference())),
        );
        p.insert("cap".into(), dv_int(MAX_INCIDENT_EDGES as i64));
        // The guard is part of the same mutable CozoScript transaction as the
        // canonical edge and anchor writes. An exact-pair update is admitted at
        // the ceiling; a genuinely new pair requires spare capacity at both
        // distinct endpoints. SQLite write conflicts make `run_db` retry this
        // whole script, so concurrent writers cannot both commit from the same
        // stale degree observation.
        let guard = "{incident_from[edge_from, edge_to] := \
               *edge{from: edge_from, to: edge_to}, edge_from == $from\n\
             incident_from[edge_from, edge_to] := \
               *edge{from: edge_from, to: edge_to}, edge_to == $from\n\
             from_degree[count(edge_from)] := incident_from[edge_from, edge_to]\n\
             incident_to[edge_from, edge_to] := \
               *edge{from: edge_from, to: edge_to}, edge_from == $to\n\
             incident_to[edge_from, edge_to] := \
               *edge{from: edge_from, to: edge_to}, edge_to == $to\n\
             to_degree[count(edge_from)] := incident_to[edge_from, edge_to]\n\
             existing_count[count(edge_from)] := \
               *edge{from: edge_from, to: edge_to}, \
               edge_from == $from, edge_to == $to\n\
             admissible[n] := existing_count[n], n > 0\n\
             admissible[from_n] := from_degree[from_n], to_degree[to_n], \
               from_n < $cap, to_n < $cap\n\
             ?[ok] := admissible[ok] :assert some}\n";
        // Edge and anchor are one logical record split only for schema
        // compatibility; update both projections in one transaction.
        let result = match edge.anchor {
            Some(s) => {
                p.insert("start".into(), dv_int(i64::from(s.start)));
                p.insert("end".into(), dv_int(i64::from(s.end)));
                let script = [
                    guard,
                    "{?[from, to, weight, kind, last_reinforced, trials, interference] <- \
                       [[$from, $to, $weight, $kind, $last_reinforced, $trials, $interference]] \
                       :put edge {from, to => weight, kind, last_reinforced, trials, interference}}\n\
                     {?[from, to, start, end] <- [[$from, $to, $start, $end]] \
                       :put edge_anchor {from, to => start, end}}",
                ]
                .concat();
                self.run(&script, p, true)
            }
            None => {
                let script = [
                    guard,
                    "{?[from, to, weight, kind, last_reinforced, trials, interference] <- \
                       [[$from, $to, $weight, $kind, $last_reinforced, $trials, $interference]] \
                       :put edge {from, to => weight, kind, last_reinforced, trials, interference}}\n\
                     {?[from, to, start, end] := *edge_anchor{from, to, start, end}, \
                       from == $from, to == $to :rm edge_anchor {from, to}}",
                ]
                .concat();
                self.run(&script, p, true)
            }
        };
        result.map_err(|error| match error {
            Error::Backend(message) if message.contains("asserted to return some") => {
                crate::incident_edge_capacity_error()
            }
            other => other,
        })?;
        Ok(())
    }

    async fn delete_edge(&self, from: NodeId, to: NodeId) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("from".into(), dv_str(&from.0.to_string()));
        p.insert("to".into(), dv_str(&to.0.to_string()));
        // Bind the stored columns so the row is fully specified for :rm.
        self.run(
            "{?[from, to, weight, kind, last_reinforced, trials, interference] := \
               *edge{from, to, weight, kind, last_reinforced, trials, interference}, \
               from == $from, to == $to :rm edge {from, to}}\n\
             {?[from, to, start, end] := *edge_anchor{from, to, start, end}, \
               from == $from, to == $to :rm edge_anchor {from, to}}",
            p,
            true,
        )?;
        Ok(())
    }

    async fn get_edge(&self, from: NodeId, to: NodeId) -> Result<Option<Edge>> {
        let mut p = BTreeMap::new();
        p.insert("from".into(), dv_str(&from.0.to_string()));
        p.insert("to".into(), dv_str(&to.0.to_string()));
        let rows = self
            .run_async(
                "?[weight, kind, last_reinforced, trials, interference] := \
                 *edge{from: $from, to: $to, weight, kind, last_reinforced, trials, interference}"
                    .into(),
                p,
                false,
            )
            .await?;
        match rows.rows.into_iter().next() {
            None => Ok(None),
            Some(row) => {
                let mut edge = graph_records::row_to_edge(from, to, &row)?;
                edge.anchor = self.get_anchor(from, to).await?;
                Ok(Some(edge))
            }
        }
    }

    async fn all_edges(&self, _: ColdPath) -> Result<Vec<Edge>> {
        let rows = self.run(
            "?[from, to, weight, kind, last_reinforced, trials, interference] := \
             *edge{from, to, weight, kind, last_reinforced, trials, interference}",
            BTreeMap::new(),
            false,
        )?;
        let anchors = self.anchor_map()?;
        let mut out = Vec::with_capacity(rows.rows.len());
        for row in &rows.rows {
            let from = node_id(want_str(&row[0])?)?;
            let to = node_id(want_str(&row[1])?)?;
            let mut edge = graph_records::row_to_edge(from, to, &row[2..])?;
            edge.anchor = anchors.get(&(from, to)).copied();
            out.push(edge);
        }
        Ok(out)
    }

    async fn maintenance_edge_upper_bound(
        &self,
        _: ColdPath,
    ) -> Result<Option<MaintenanceEdgeKey>> {
        let page = self
            .scan_primary_key_async(
                "edge",
                PrimaryKeyScan {
                    prefix: Vec::new(),
                    lower: PrimaryKeyScanBound::Unbounded,
                    upper: PrimaryKeyScanBound::Unbounded,
                    direction: PrimaryKeyScanDirection::Descending,
                    limit: 1,
                },
            )
            .await?;
        page.rows
            .rows
            .first()
            .map(|row| {
                Ok(MaintenanceEdgeKey::new(
                    node_id(want_str(&row[0])?)?,
                    node_id(want_str(&row[1])?)?,
                ))
            })
            .transpose()
    }

    async fn maintenance_edges_page(
        &self,
        _: ColdPath,
        after: Option<MaintenanceEdgeKey>,
        through: MaintenanceEdgeKey,
        limit: usize,
    ) -> Result<MaintenanceEdgePage> {
        crate::validate_maintenance_page_limit(limit)?;
        if after.is_some_and(|after| after >= through) {
            return Ok(MaintenanceEdgePage {
                items: Vec::new(),
                next: None,
            });
        }
        let encode_key = |key: MaintenanceEdgeKey| {
            vec![
                dv_str(&key.from.0.to_string()),
                dv_str(&key.to.0.to_string()),
            ]
        };
        let page = self
            .scan_primary_key_async(
                "edge",
                PrimaryKeyScan {
                    prefix: Vec::new(),
                    lower: after.map_or(PrimaryKeyScanBound::Unbounded, |after| {
                        PrimaryKeyScanBound::Excluded(encode_key(after))
                    }),
                    upper: PrimaryKeyScanBound::Included(encode_key(through)),
                    direction: PrimaryKeyScanDirection::Ascending,
                    limit: limit + 1,
                },
            )
            .await?;
        let rows = page.rows;
        let mut items = Vec::with_capacity(rows.rows.len());
        for row in &rows.rows {
            let from = node_id(want_str(&row[0])?)?;
            let to = node_id(want_str(&row[1])?)?;
            items.push(graph_records::row_to_edge(from, to, &row[2..])?);
        }
        let mut page = crate::finish_maintenance_edge_page(items, limit);
        let pairs: Vec<_> = page.items.iter().map(|edge| (edge.from, edge.to)).collect();
        let anchors = self.anchors_for(&pairs).await?;
        for edge in &mut page.items {
            edge.anchor = anchors.get(&(edge.from, edge.to)).copied();
        }
        Ok(page)
    }

    async fn incident_edges_page(
        &self,
        request: &IncidentEdgesRequest,
    ) -> Result<Option<IncidentEdgesPage>> {
        request.validate()?;
        let request = request.clone();
        let started = std::time::Instant::now();
        let job = ActiveBackendJob {
            db: self.db.clone(),
            authority: self.persistent_authority.clone(),
            activity: self.backend_activity.enter(),
        };
        tokio::task::spawn_blocking(move || job.run(|db| {
            // Queue/authority time consumes the caller's shared remaining time.
            let remaining = crate::linked_read_remaining(started, request.remaining())?;
            let mut tx = db.read_multi_transaction_with_timeout(remaining).map_err(backend)?;
            let result = crate::collect_incident_edges_page(&request, |leg, after, quota, work| {
                crate::linked_read_remaining(started, request.remaining())?;
                work.indexed_seeks += 1;
                let relation = match leg { IncidentEdgeLeg::Outgoing => "edge", IncidentEdgeLeg::Incoming => "edge:by_to" };
                let lower = after.map_or(PrimaryKeyScanBound::Unbounded, |key| {
                    let endpoint = match leg { IncidentEdgeLeg::Outgoing => key.to, IncidentEdgeLeg::Incoming => key.from };
                    PrimaryKeyScanBound::Excluded(vec![dv_str(&endpoint.0.to_string())])
                });
                let rows = tx.scan_relation_by_primary_key(relation, PrimaryKeyScan {
                    prefix: vec![dv_str(&request.anchor().0.to_string())],
                    lower, upper: PrimaryKeyScanBound::Unbounded,
                    direction: PrimaryKeyScanDirection::Ascending, limit: quota,
                }).map_err(backend)?.rows;
                if rows.len() > quota { return Err(backend_str("incident index exceeded admitted row quota".into())); }
                let mut items = Vec::with_capacity(rows.len());
                for row in rows {
                    crate::linked_read_remaining(started, request.remaining())?;
                    if row.len() < 2 { return Err(backend_str("malformed incident index row".into())); }
                    let first = node_id(want_str(&row[0])?)?;
                    let second = node_id(want_str(&row[1])?)?;
                    let (from, to) = match leg { IncidentEdgeLeg::Outgoing => (first, second), IncidentEdgeLeg::Incoming => (second, first) };
                    let params = BTreeMap::from([("from".into(), dv_str(&from.0.to_string())), ("to".into(), dv_str(&to.0.to_string()))]);
                    let mut edge = match leg {
                        IncidentEdgeLeg::Outgoing => graph_records::row_to_edge(from, to, &row[2..])?,
                        IncidentEdgeLeg::Incoming => {
                            work.edge_point_reads += 1;
                            let canonical = tx.run_script("?[weight, kind, last_reinforced, trials, interference] := *edge{from: $from, to: $to, weight, kind, last_reinforced, trials, interference}", params.clone()).map_err(backend)?;
                            if canonical.rows.len() != 1 { return Err(backend_str("incident incoming index has no unique canonical edge".into())); }
                            graph_records::row_to_edge(from, to, &canonical.rows[0])?
                        }
                    };
                    crate::linked_read_remaining(started, request.remaining())?;
                    work.body_anchor_point_reads += 1;
                    let anchors = tx.run_script("?[start, end] := *edge_anchor{from: $from, to: $to, start, end}", params).map_err(backend)?;
                    if anchors.rows.len() > 1 { return Err(backend_str("incident edge has duplicate anchors".into())); }
                    edge.anchor = anchors.rows.first().map(|row| {
                        if row.len() != 2 { return Err(backend_str("malformed incident body anchor".into())); }
                        graph_records::decode_body_span(&row[0], &row[1])
                    }).transpose()?;
                    items.push(edge);
                }
                Ok(items)
            }).and_then(|page| {
                crate::linked_read_remaining(started, request.remaining())?;
                Ok(Some(page))
            });
            let close = tx.close_and_join().map_err(backend);
            match (result, close) {
                (Ok(value), Ok(())) => Ok(value),
                (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
                (Err(error), Err(close)) => Err(backend_str(format!("incident read failed ({error}); snapshot teardown failed ({close})"))),
            }
        })).await.map_err(|error| backend_str(format!("join incident read worker: {error}")))?
    }

    async fn neighbors(&self, id: NodeId, top_k: usize) -> Result<Vec<Neighbor>> {
        if top_k == 0 {
            return Ok(Vec::new());
        }

        // Fetch every edge incident to `id` (either endpoint), then orient by
        // kind in Rust — the same rule as the reference store. (Bind endpoints as
        // free variables and filter with `==`; a `{from: $id}` match wouldn't.)
        let script = "inc[from, to, weight, kind, lr, tr, intf] := *edge{from, to, weight, kind, last_reinforced: lr, trials: tr, interference: intf}, from == $id\n\
             inc[from, to, weight, kind, lr, tr, intf] := *edge{from, to, weight, kind, last_reinforced: lr, trials: tr, interference: intf}, to == $id\n\
             ?[from, to, weight, kind, lr, tr, intf] := inc[from, to, weight, kind, lr, tr, intf] :limit $read_cap";
        let mut p = BTreeMap::new();
        p.insert("id".into(), dv_str(&id.0.to_string()));
        p.insert("read_cap".into(), dv_int((MAX_INCIDENT_EDGES + 1) as i64));
        let rows = self.run_async(script.into(), p, false).await?;
        if rows.rows.len() > MAX_INCIDENT_EDGES {
            return Err(crate::incident_edge_capacity_error());
        }

        let mut edges = Vec::with_capacity(rows.rows.len());
        for row in &rows.rows {
            let from = node_id(want_str(&row[0])?)?;
            let to = node_id(want_str(&row[1])?)?;
            edges.push(graph_records::row_to_edge(from, to, &row[2..])?);
        }
        let mut out = crate::oriented_neighbors(id, &edges);
        let mut ids = out.iter().map(|neighbor| neighbor.node).collect::<Vec<_>>();
        ids.push(id);
        ids.sort_unstable();
        ids.dedup();
        // Raw neighbors historically exposes dangling edge endpoints. Preserve
        // that inspection contract, while excluding known episode facets before
        // top-k. Scoped traversal separately requires an existing semantic node.
        let (input, params) = id_input("neighbor_kind", &ids);
        let rows = self
            .run_async(
                format!(
                    "{input}\n?[id, data, status] := neighbor_kind[id], *node{{id, data, status}}"
                ),
                params,
                false,
            )
            .await?;
        let mut episodes = HashSet::new();
        for row in &rows.rows {
            let node = decode_canonical_node_row(row)?;
            if !node.is_semantic() {
                episodes.insert(node.id());
            }
        }
        if episodes.contains(&id) {
            return Ok(Vec::new());
        }
        out.retain(|neighbor| !episodes.contains(&neighbor.node));
        out.sort_by(crate::neighbor_order);
        out.truncate(top_k);

        let pairs: Vec<_> = out
            .iter()
            .map(|neighbor| (neighbor.edge.from, neighbor.edge.to))
            .collect();
        let anchors = self.anchors_for(&pairs).await?;
        for neighbor in &mut out {
            neighbor.edge.anchor = anchors
                .get(&(neighbor.edge.from, neighbor.edge.to))
                .copied();
        }
        Ok(out)
    }

    async fn commit_feedback(&self, commit: &FeedbackCommit) -> Result<FeedbackCommitOutcome> {
        commit.validate()?;
        if let Some(authority) = &self.persistent_authority {
            authority.require_active_feedback_epoch(
                commit
                    .idempotency
                    .as_ref()
                    .map(|idempotency| idempotency.retry.epoch.as_str()),
            )?;
        }
        #[cfg(test)]
        self.query_count.fetch_add(1, AtomicOrdering::Relaxed);

        // Rust computes the domain delta; Cozo owns its one true commit point.
        // Dropping/aborting this handle rolls back every script below, including
        // the retry proof, while `commit()` makes all rows visible together.
        let tx = self.db.multi_transaction(true);
        let staged = stage_feedback_transaction(&tx, commit);
        match staged {
            Ok(FeedbackCommitOutcome::AlreadyApplied) => {
                tx.abort().map_err(backend)?;
                Ok(FeedbackCommitOutcome::AlreadyApplied)
            }
            Ok(FeedbackCommitOutcome::Applied) => {
                tx.commit().map_err(backend)?;
                Ok(FeedbackCommitOutcome::Applied)
            }
            Err(error) => {
                let _ = tx.abort();
                Err(error)
            }
        }
    }

    async fn commit_full_merge(&self, commit: &FullMergeCommit) -> Result<FullMergeCommitOutcome> {
        commit.validate()?;
        #[cfg(test)]
        self.query_count.fetch_add(1, AtomicOrdering::Relaxed);

        let tx = self.db.multi_transaction(true);
        let staged = stage_full_merge_transaction(&tx, self.db_id, commit);
        match staged {
            Ok(FullMergeCommitOutcome::AlreadyApplied) => {
                tx.abort().map_err(backend)?;
                Ok(FullMergeCommitOutcome::AlreadyApplied)
            }
            Ok(FullMergeCommitOutcome::Applied) => {
                tx.commit().map_err(backend)?;
                Ok(FullMergeCommitOutcome::Applied)
            }
            Err(error) => {
                let _ = tx.abort();
                Err(error)
            }
        }
    }

    async fn commit_supersede(&self, commit: &SupersedeCommit) -> Result<SupersedeCommitOutcome> {
        commit.validate()?;
        #[cfg(test)]
        self.query_count.fetch_add(1, AtomicOrdering::Relaxed);

        let tx = self.db.multi_transaction(true);
        let staged = stage_supersede_transaction(&tx, commit);
        match staged {
            Ok(SupersedeCommitOutcome::AlreadyApplied) => {
                tx.abort().map_err(backend)?;
                Ok(SupersedeCommitOutcome::AlreadyApplied)
            }
            Ok(SupersedeCommitOutcome::Applied) => {
                tx.commit().map_err(backend)?;
                Ok(SupersedeCommitOutcome::Applied)
            }
            Err(error) => {
                let _ = tx.abort();
                Err(error)
            }
        }
    }

    async fn commit_maintenance(
        &self,
        _: ColdPath,
        commit: &MaintenanceCommit,
    ) -> Result<MaintenanceCommitOutcome> {
        commit.validate()?;
        #[cfg(test)]
        self.query_count.fetch_add(1, AtomicOrdering::Relaxed);
        let commit = commit.clone();
        let job = ActiveBackendJob {
            db: self.db.clone(),
            authority: self.persistent_authority.clone(),
            activity: self.backend_activity.enter(),
        };
        let staged = tokio::task::spawn_blocking(move || {
            job.run(|db| run_maintenance_transaction(db, &commit))
        })
        .await
        .map_err(|error| backend_str(format!("join maintenance worker: {error}")))??;
        let MaintenanceStageOutcome {
            outcome,
            statements,
        } = staged;
        #[cfg(test)]
        self.last_maintenance_statements
            .store(statements, AtomicOrdering::Relaxed);
        #[cfg(not(test))]
        let _ = statements;
        Ok(outcome)
    }

    async fn maintenance_overfull_hubs(
        &self,
        _: ColdPath,
        candidates: &[NodeId],
        target_degree: usize,
    ) -> Result<Vec<NodeId>> {
        if candidates.len() > MAX_MAINTENANCE_BATCH_ROWS {
            return Err(Error::CapacityExceeded {
                resource: "maintenance hub preflight",
                limit: MAX_MAINTENANCE_BATCH_ROWS,
            });
        }
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let Ok(target_degree) = i64::try_from(target_degree) else {
            return Ok(Vec::new());
        };
        let (input, mut params) = id_input("candidate_hub", candidates);
        params.insert("target".into(), dv_int(target_degree));
        params.insert("fetch".into(), dv_int((candidates.len() + 1) as i64));
        // The first-stage rules bind only a physical/index key prefix. Keep the
        // kind filter in a second in-memory rule: putting `kind: 'associative'`
        // directly beside `from: hub` makes Cozo choose `stored_mat_join` and
        // materialize all edges because `(from, kind)` is not a contiguous key.
        // With at most 64 candidate hubs and the hard incident cap, the two
        // prefix joins bound aggregation input independently of E.
        let rows = self
            .run_async(maintenance_overfull_hubs_query(&input), params, false)
            .await?;
        if rows.rows.len() > candidates.len() {
            return Err(backend_str(
                "maintenance hub preflight returned more rows than candidates".into(),
            ));
        }
        let candidates: HashSet<_> = candidates.iter().copied().collect();
        let mut seen = HashSet::new();
        let mut hubs = Vec::with_capacity(rows.rows.len());
        for row in &rows.rows {
            let id = node_id(want_str(&row[0])?)?;
            if !candidates.contains(&id) || !seen.insert(id) {
                return Err(backend_str(format!(
                    "maintenance hub preflight returned invalid id {id:?}"
                )));
            }
            hubs.push(id);
        }
        hubs.sort_unstable();
        Ok(hubs)
    }

    async fn prune_incident_associations(
        &self,
        _: ColdPath,
        hub: NodeId,
        target_degree: usize,
        max_deletes: usize,
    ) -> Result<DensePruneChunkOutcome> {
        if !(1..=MAX_MAINTENANCE_BATCH_ROWS).contains(&max_deletes) {
            return Err(Error::InvalidInput(format!(
                "maintenance prune limit must be in 1..={MAX_MAINTENANCE_BATCH_ROWS}"
            )));
        }
        #[cfg(test)]
        self.query_count.fetch_add(1, AtomicOrdering::Relaxed);
        let job = ActiveBackendJob {
            db: self.db.clone(),
            authority: self.persistent_authority.clone(),
            activity: self.backend_activity.enter(),
        };
        tokio::task::spawn_blocking(move || {
            job.run(|db| {
                let tx = db.multi_transaction(true);
                match stage_dense_prune_transaction(&tx, hub, target_degree, max_deletes) {
                    Ok(outcome) => {
                        tx.commit().map_err(backend)?;
                        Ok(outcome)
                    }
                    Err(error) => {
                        let _ = tx.abort();
                        Err(error)
                    }
                }
            })
        })
        .await
        .map_err(|error| backend_str(format!("join dense-prune worker: {error}")))?
    }

    async fn observe_contradiction(&self, a: NodeId, b: NodeId, at: Timestamp) -> Result<()> {
        let between = UnorderedPair(a, b);
        Contradiction::new(a, b, at)
            .validate()
            .map_err(Error::InvalidInput)?;
        let tx = self.db.multi_transaction(true);
        let staged = (|| {
            let updated = match overlay::load_contradiction(&tx, between)? {
                Some(mut contradiction) => {
                    contradiction.observe(at);
                    contradiction
                }
                None => Contradiction::new(a, b, at),
            };
            overlay::stage_contradiction_upsert(&tx, &updated)
        })();
        overlay::finish_transaction(&tx, staged)
    }

    async fn open_contradictions(&self, _: ColdPath) -> Result<Vec<Contradiction>> {
        let rows = self.run(
            "?[lo, hi, observations, first_seen, last_seen, resolution] := \
             *contradiction{lo, hi, observations, first_seen, last_seen, resolution}, is_null(resolution)\n\
             ?[lo, hi, observations, first_seen, last_seen, resolution] := \
             *contradiction{lo, hi, observations, first_seen, last_seen, resolution}, resolution == 'unresolved'",
            BTreeMap::new(),
            false,
        )?;
        rows.rows
            .iter()
            .map(|row| {
                let between = overlay::decode_stored_pair(&row[0], &row[1], "contradiction")?;
                let values = row.get(2..6).ok_or_else(|| {
                    backend_str("stored open contradiction row is truncated".into())
                })?;
                overlay::decode_contradiction_values(between, values)
            })
            .collect()
    }

    async fn resolve_contradiction(
        &self,
        pair: UnorderedPair<NodeId>,
        resolution: Resolution,
    ) -> Result<()> {
        let (lo, hi) = canonical(pair);
        let tx = self.db.multi_transaction(true);
        let resolved = (|| {
            tx_reject_episode_nodes(&tx, &[pair.0, pair.1], "resolve reconciliation")?;
            let mut params = BTreeMap::new();
            params.insert("lo".into(), dv_str(&lo));
            params.insert("hi".into(), dv_str(&hi));
            let rows = tx_run(
                &tx,
                "?[observations, first_seen, last_seen, resolution] := \
                 *contradiction{lo: $lo, hi: $hi, observations, first_seen, last_seen, resolution}",
                params.clone(),
            )?;
            let row = rows.rows.first().ok_or(Error::NotFound)?;
            let current = overlay::decode_contradiction_values(pair, row)?;
            match current.resolution {
                Some(existing) if existing == resolution => return Ok(false),
                Some(Resolution::Unresolved) => {}
                Some(_) => {
                    return Err(Error::Conflict(
                        "contradiction already has a different terminal resolution".into(),
                    ));
                }
                None => {}
            }
            params.insert("requested".into(), dv_str(resolution_str(resolution)));
            tx_run(
                &tx,
                "?[lo, hi, observations, first_seen, last_seen, resolution] := \
                 *contradiction{lo, hi, observations, first_seen, last_seen}, \
                 lo == $lo, hi == $hi, resolution = $requested \
                 :put contradiction {lo, hi => observations, first_seen, last_seen, resolution}",
                params,
            )?;
            Ok::<_, Error>(true)
        })();
        match resolved {
            Ok(true) => tx.commit().map_err(backend),
            Ok(false) => tx.abort().map_err(backend),
            Err(error) => {
                let _ = tx.abort();
                Err(error)
            }
        }
    }

    async fn observe_merge_candidate(&self, a: NodeId, b: NodeId, at: Timestamp) -> Result<()> {
        let between = UnorderedPair(a, b);
        MergeCandidate::new(a, b, at)
            .validate()
            .map_err(Error::InvalidInput)?;
        let tx = self.db.multi_transaction(true);
        let staged = (|| {
            let updated = match overlay::load_merge_candidate(&tx, between)? {
                Some(mut candidate) => {
                    candidate.observe(at);
                    candidate
                }
                None => MergeCandidate::new(a, b, at),
            };
            overlay::stage_merge_candidate_upsert(&tx, &updated)
        })();
        overlay::finish_transaction(&tx, staged)
    }

    async fn open_merge_candidates(&self, _: ColdPath) -> Result<Vec<MergeCandidate>> {
        let rows = self.run(
            "?[lo, hi, observations, first_seen, last_seen, resolution] := \
             *merge_candidate{lo, hi, observations, first_seen, last_seen, resolution}, is_null(resolution)",
            BTreeMap::new(),
            false,
        )?;
        rows.rows
            .iter()
            .map(|row| {
                let between = overlay::decode_stored_pair(&row[0], &row[1], "merge candidate")?;
                let values = row.get(2..6).ok_or_else(|| {
                    backend_str("stored open merge candidate row is truncated".into())
                })?;
                overlay::decode_merge_candidate_values(between, values)
            })
            .collect()
    }

    async fn resolve_merge_candidate(
        &self,
        pair: UnorderedPair<NodeId>,
        resolution: MergeResolution,
    ) -> Result<()> {
        let (lo, hi) = canonical(pair);
        let tx = self.db.multi_transaction(true);
        let resolved = (|| {
            tx_reject_episode_nodes(&tx, &[pair.0, pair.1], "resolve reconciliation")?;
            let mut params = BTreeMap::new();
            params.insert("lo".into(), dv_str(&lo));
            params.insert("hi".into(), dv_str(&hi));
            let rows = tx_run(
                &tx,
                "?[observations, first_seen, last_seen, resolution] := \
                   *merge_candidate{lo: $lo, hi: $hi, observations, first_seen, last_seen, resolution}",
                params.clone(),
            )?;
            let row = rows.rows.first().ok_or(Error::NotFound)?;
            let current = overlay::decode_merge_candidate_values(pair, row)?;
            match current.resolution {
                Some(existing) if existing == resolution => return Ok(false),
                Some(_) => {
                    return Err(Error::Conflict(
                        "merge candidate already has a different terminal resolution".into(),
                    ));
                }
                None => {}
            }

            params.insert("requested".into(), dv_str(merge_resolution_str(resolution)));
            tx_run(
                &tx,
                "?[lo, hi, observations, first_seen, last_seen, resolution] := \
                   *merge_candidate{lo, hi, observations, first_seen, last_seen}, \
                   lo == $lo, hi == $hi, resolution = $requested \
                   :put merge_candidate \
                     {lo, hi => observations, first_seen, last_seen, resolution}",
                params,
            )?;
            Ok(true)
        })();
        match resolved {
            Ok(true) => tx.commit().map_err(backend),
            Ok(false) => {
                tx.abort().map_err(backend)?;
                Ok(())
            }
            Err(error) => {
                let _ = tx.abort();
                Err(error)
            }
        }
    }

    async fn put_remote_edge(&self, edge: &RemoteEdge) -> Result<()> {
        self.upsert_remote_edge_for_source_database(edge, self.db_id)
    }

    async fn remote_edges_page(
        &self,
        from: NodeId,
        after: Option<RemoteEdgeCursor>,
        limit: usize,
    ) -> Result<RemoteEdgePage> {
        crate::validate_remote_edge_page_request(from, after, limit)?;
        let mut p = BTreeMap::new();
        p.insert("from".into(), dv_str(&from.0.to_string()));
        // Aggregate rather than materialize a 257-row canary. This catches a
        // legacy/corrupt store that bypassed the source-degree write guard.
        let stats = self.run(
            "source[target_db, target, weight] := \
                 *remote_edge{from: $from, target_db, target, weight}\n\
             all_count[count(target)] := source[target_db, target, weight]\n\
             valid_count[count(target)] := source[target_db, target, weight], \
                 is_finite(weight), weight >= 0, weight <= 1\n\
             ?[all, valid] := all_count[all], valid_count[valid]",
            p.clone(),
            false,
        )?;
        let stats = stats
            .rows
            .first()
            .ok_or_else(|| backend_str("remote edge stats query returned no row".into()))?;
        let degree = want_i64(&stats[0])?;
        let valid_degree = want_i64(&stats[1])?;
        if degree < 0 || degree as usize > MAX_REMOTE_EDGES_PER_SOURCE {
            return Err(crate::remote_edge_source_capacity_error());
        }
        if valid_degree != degree {
            return Err(backend_str(
                "remote edge source contains a non-finite or out-of-range weight".into(),
            ));
        }

        p.insert("fetch".into(), dv_int((limit + 1) as i64));
        let script = if let Some(cursor) = after {
            p.insert("after_weight".into(), dv_float(cursor.weight() as f64));
            p.insert(
                "after_target_db".into(),
                dv_str(&cursor.target_db().to_string()),
            );
            p.insert(
                "after_target".into(),
                dv_str(&cursor.target().0.to_string()),
            );
            "page[target_db, target, weight] := \
                 *remote_edge{from: $from, target_db, target, weight}, \
                 weight < $after_weight\n\
             page[target_db, target, weight] := \
                 *remote_edge{from: $from, target_db, target, weight}, \
                 weight == $after_weight, target_db > $after_target_db\n\
             page[target_db, target, weight] := \
                 *remote_edge{from: $from, target_db, target, weight}, \
                 weight == $after_weight, target_db == $after_target_db, \
                 target > $after_target\n\
             ?[target_db, target, weight] := page[target_db, target, weight] \
             :order -weight, target_db, target :limit $fetch"
        } else {
            "?[target_db, target, weight] := \
                 *remote_edge{from: $from, target_db, target, weight} \
             :order -weight, target_db, target :limit $fetch"
        };
        // The relation's primary key is `(from, target_db, target)`, not this
        // presentation order. Cozo therefore scans/sorts at most the hard
        // source cap (256) internally, but the query result and Rust allocation
        // are bounded to page-size + 1. A weight-leading covering index is a
        // future schema optimization, not a prerequisite for bounded memory.
        let rows = self.run(script, p, false)?;
        let out: Vec<RemoteEdge> = rows
            .rows
            .iter()
            .map(|row| {
                graph_records::decode_remote_edge(self.db_id, from, &row[0], &row[1], &row[2])
            })
            .collect::<Result<_>>()?;
        Ok(crate::finish_remote_edge_page(out, limit))
    }

    async fn delete_remote_edge(
        &self,
        from: NodeId,
        target_db: Ulid,
        target: NodeId,
    ) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("from".into(), dv_str(&from.0.to_string()));
        p.insert("target_db".into(), dv_str(&target_db.to_string()));
        p.insert("target".into(), dv_str(&target.0.to_string()));
        self.run(
            "?[from, target_db, target] := *remote_edge{from, target_db, target}, \
             from == $from, target_db == $target_db, target == $target \
             :rm remote_edge {from, target_db, target}",
            p,
            true,
        )?;
        Ok(())
    }
}

/// The canonical facet, not lifecycle status, decides whether a generic
/// semantic mutation may touch a node. Canonical rows remain complete for
/// export and exact historical get operations.
pub(super) fn require_semantic_node(node: &Node, operation: &str) -> Result<()> {
    if !node.is_semantic() {
        return Err(Error::InvalidInput(format!(
            "{operation} cannot modify episode editions; use the episode editorial operation"
        )));
    }
    Ok(())
}

/// Bounded point reads in an existing transaction. Missing endpoints are left to
/// each operation's existing existence/CAS contract; only typed editions are
/// returned. Chunking bounds generated scripts at the incident-edge ceiling.
pub(super) fn tx_episode_ids(tx: &MultiTransaction, ids: &[NodeId]) -> Result<HashSet<NodeId>> {
    let ids = ids
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut episodes = HashSet::new();
    for chunk in ids.chunks(128) {
        let (input, params) = id_input("facet_key", chunk);
        let rows = tx_run(
            tx,
            &format!("{input}\n?[id, data, status] := facet_key[id], *node{{id, data, status}}"),
            params,
        )?;
        for row in &rows.rows {
            let node = decode_canonical_node_row(row)?;
            if !node.is_semantic() {
                episodes.insert(node.id());
            }
        }
    }
    Ok(episodes)
}

pub(super) fn tx_reject_episode_nodes(
    tx: &MultiTransaction,
    ids: &[NodeId],
    operation: &str,
) -> Result<()> {
    if !tx_episode_ids(tx, ids)?.is_empty() {
        return Err(Error::InvalidInput(format!(
            "{operation} cannot modify episode editions or their evidence links"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod episode_guard_tests {
    use super::*;
    use mneme_core::episode::{EpisodeFacet, EpisodeTime, OccurrenceSpan};
    use mneme_core::ports::{FeedbackEdgeUpdate, FeedbackNodeUpdate};
    use mneme_core::{BodyRef, CaptureReplayProof, CaptureRequestCodec, CaptureSource};

    fn semantic(id: u128) -> Node {
        Node::try_new(
            NodeId(Ulid::from(id)),
            "semantic lesson",
            BodyRef::new("inline://lesson").unwrap(),
            std::iter::empty::<&str>(),
            Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Active,
            1,
        )
        .unwrap()
    }

    fn episode() -> Node {
        let source = CaptureSource::new_with_codec(
            "episode-guard-test",
            "initial",
            "synthetic://scene",
            None::<&str>,
            None::<&str>,
            [7; 32],
            CaptureRequestCodec::EpisodeV1,
        )
        .unwrap();
        let id = source.node_id();
        Node::try_new(
            id,
            "a scene, not a lesson",
            BodyRef::new("inline://scene").unwrap(),
            std::iter::empty::<&str>(),
            Provenance::External { source },
            0.5,
            0.5,
            NodeStatus::Active,
            1,
        )
        .unwrap()
        .with_episode(
            EpisodeFacet::initial(
                id,
                OccurrenceSpan::Unknown,
                None,
                EpisodeTime::new(1).unwrap(),
            )
            .unwrap(),
        )
        .unwrap()
    }

    // A kernel fixture for generic mutation/read isolation, not an episode
    // publication fixture: the canonical facet is valid but no head/index is
    // required by the tested operations. Episode commit/read tests own those.
    fn insert_episode_fixture(store: &CozoStore, node: &Node) {
        let mut p = BTreeMap::new();
        p.insert("id".into(), dv_str(&node.id().0.to_string()));
        p.insert("data".into(), dv_str(&encode_canonical_node(node).unwrap()));
        store
            .run(
                "?[id, data, status] <- [[$id, $data, 'active']] :put node {id => data, status}",
                p,
                true,
            )
            .unwrap();
    }

    fn assert_episode_refusal<T: std::fmt::Debug>(result: Result<T>) {
        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("episode"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn generic_writes_refuse_episode_editions_without_a_prefix_mutation() {
        let store = CozoStore::new(4).unwrap();
        let episode = episode();
        let lesson = semantic(1);
        store.put_node(&lesson).await.unwrap();
        assert_episode_refusal(store.put_node(&episode).await);
        assert!(store.get_node(episode.id()).await.unwrap().is_none());
        insert_episode_fixture(&store, &episode);
        assert_episode_refusal(
            store
                .compare_replace_node_body(episode.id(), &episode.body_revision(), lesson.body())
                .await,
        );
        let before = serde_json::to_value(&episode).unwrap();
        let mut flattened = semantic(2);
        // Rebuild a semantic value at the immutable edition's exact identity.
        flattened = Node::try_new(
            episode.id(),
            flattened.summary(),
            flattened.body().clone(),
            std::iter::empty::<&str>(),
            Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        assert_episode_refusal(store.put_node(&flattened).await);
        assert_episode_refusal(store.delete_node(episode.id()).await);
        assert_episode_refusal(store.set_status(episode.id(), NodeStatus::Archived).await);
        assert_episode_refusal(
            store
                .compare_replace_node_tags(episode.id(), episode.tag_set(), episode.tag_set())
                .await,
        );
        let Provenance::External { source } = episode.provenance() else {
            unreachable!()
        };
        let proof = CaptureReplayProof::episode(source.clone()).unwrap();
        assert_episode_refusal(
            store
                .commit_capture(&episode, &[1.0, 0.0, 0.0, 0.0], &proof)
                .await,
        );
        assert_episode_refusal(
            store
                .observe_contradiction(lesson.id(), episode.id(), 2)
                .await,
        );
        assert_episode_refusal(
            store
                .observe_merge_candidate(lesson.id(), episode.id(), 2)
                .await,
        );
        assert_episode_refusal(
            store
                .commit_supersede(&SupersedeCommit::new(lesson.id(), episode.id(), 2).unwrap())
                .await,
        );
        assert_episode_refusal(
            store
                .commit_full_merge(&FullMergeCommit::new(lesson.id(), episode.id(), 2).unwrap())
                .await,
        );
        let mut revised = episode.clone();
        revised.record_grounded_use(2);
        assert_episode_refusal(
            store
                .commit_feedback(&FeedbackCommit {
                    idempotency: None,
                    applied_at: 2,
                    nodes: vec![FeedbackNodeUpdate {
                        expected: episode.clone(),
                        replacement: revised.clone(),
                    }],
                    edges: Vec::new(),
                    merge_observations: Vec::new(),
                })
                .await,
        );
        assert_eq!(
            serde_json::to_value(store.get_node(episode.id()).await.unwrap().unwrap()).unwrap(),
            before
        );
        assert_eq!(store.all_nodes(ColdPath::acquire()).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn semantic_neighbors_and_traversal_ignore_episodes_before_budgets() {
        let store = CozoStore::new(4).unwrap();
        let a = semantic(10);
        let b = semantic(11);
        let hidden = episode();
        store.put_node(&a).await.unwrap();
        store.put_node(&b).await.unwrap();
        insert_episode_fixture(&store, &hidden);
        for edge in [
            Edge::new(a.id(), hidden.id(), 1.0, EdgeKind::Associative, 1),
            Edge::new(hidden.id(), b.id(), 1.0, EdgeKind::Associative, 1),
            Edge::new(a.id(), b.id(), 0.2, EdgeKind::Associative, 1),
        ] {
            store.put_edge(&edge).await.unwrap();
        }
        assert_eq!(store.neighbors(a.id(), 1).await.unwrap()[0].node, b.id());
        assert!(store.neighbors(hidden.id(), 8).await.unwrap().is_empty());
        let result = store
            .spread(
                &[
                    Scored {
                        id: hidden.id(),
                        score: 100.0,
                    },
                    Scored {
                        id: a.id(),
                        score: 1.0,
                    },
                ],
                Budget {
                    max_nodes: 2,
                    max_depth: 2,
                    explore: 0.0,
                    query_conditioning: 0.0,
                    ..Budget::default()
                },
                None,
                TraversalScope::new(StatusFilter::ALL),
            )
            .await
            .unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|hit| hit.id != hidden.id()));
        assert_eq!(
            store
                .detect_communities(ColdPath::acquire())
                .await
                .unwrap()
                .len(),
            2
        );
        assert_eq!(store.all_edges(ColdPath::acquire()).await.unwrap().len(), 3);
        // Without the direct semantic link the episode cannot be a bridge.
        store.delete_edge(a.id(), b.id()).await.unwrap();
        let result = store
            .spread(
                &[Scored {
                    id: a.id(),
                    score: 1.0,
                }],
                Budget {
                    max_nodes: 8,
                    max_depth: 3,
                    ..Budget::default()
                },
                None,
                TraversalScope::new(StatusFilter::ALL),
            )
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
    }

    #[tokio::test]
    async fn maintenance_feedback_and_delete_preserve_authored_evidence() {
        let store = CozoStore::new(4).unwrap();
        let a = semantic(20);
        let b = semantic(21);
        let scene = episode();
        store.put_node(&a).await.unwrap();
        store.put_node(&b).await.unwrap();
        insert_episode_fixture(&store, &scene);
        let evidence = Edge::new(scene.id(), a.id(), 0.1, EdgeKind::Associative, 1);
        let speculative = Edge::new(a.id(), b.id(), 0.2, EdgeKind::Associative, 1);
        store.put_edge(&evidence).await.unwrap();
        store.put_edge(&speculative).await.unwrap();
        assert_episode_refusal(store.delete_node(a.id()).await);
        assert_episode_refusal(
            store
                .commit_maintenance(
                    ColdPath::acquire(),
                    &MaintenanceCommit {
                        edges: vec![MaintenanceEdgeMutation::DeleteWeak {
                            expected: evidence.clone(),
                        }],
                    },
                )
                .await,
        );
        let mut revised = a.clone();
        revised.record_grounded_use(2);
        assert_episode_refusal(
            store
                .commit_feedback(&FeedbackCommit {
                    idempotency: None,
                    applied_at: 2,
                    nodes: vec![FeedbackNodeUpdate {
                        expected: a.clone(),
                        replacement: revised,
                    }],
                    edges: vec![FeedbackEdgeUpdate {
                        expected: Some(evidence.clone()),
                        replacement: evidence.clone(),
                    }],
                    merge_observations: Vec::new(),
                })
                .await,
        );
        assert_eq!(
            store
                .get_node(a.id())
                .await
                .unwrap()
                .unwrap()
                .grounded_use_count(),
            a.grounded_use_count()
        );
        store
            .prune_incident_associations(ColdPath::acquire(), a.id(), 0, 1)
            .await
            .unwrap();
        assert!(store.get_edge(a.id(), b.id()).await.unwrap().is_none());
        assert_eq!(
            serde_json::to_value(store.get_edge(scene.id(), a.id()).await.unwrap()).unwrap(),
            serde_json::to_value(Some(&evidence)).unwrap()
        );
        // Deliberate authored edge edits remain available; only then can the
        // semantic endpoint be forgotten without dangling historical evidence.
        store.delete_edge(scene.id(), a.id()).await.unwrap();
        store.delete_node(a.id()).await.unwrap();
        assert!(store.get_node(a.id()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn full_merge_retains_episode_evidence_on_archived_loser_and_replays() {
        let store = CozoStore::new(4).unwrap();
        let winner = semantic(30);
        let loser = semantic(31);
        let other = semantic(32);
        let scene = episode();
        for node in [&winner, &loser, &other] {
            store.put_node(node).await.unwrap();
        }
        insert_episode_fixture(&store, &scene);
        let evidence = Edge::new(scene.id(), loser.id(), 0.7, EdgeKind::DerivedFrom, 1);
        store.put_edge(&evidence).await.unwrap();
        store
            .put_edge(&Edge::new(
                loser.id(),
                other.id(),
                0.5,
                EdgeKind::Associative,
                1,
            ))
            .await
            .unwrap();
        store
            .observe_merge_candidate(winner.id(), loser.id(), 1)
            .await
            .unwrap();
        let commit = FullMergeCommit::new(winner.id(), loser.id(), 2).unwrap();
        assert_eq!(
            store.commit_full_merge(&commit).await.unwrap(),
            FullMergeCommitOutcome::Applied
        );
        assert_eq!(
            store.commit_full_merge(&commit).await.unwrap(),
            FullMergeCommitOutcome::AlreadyApplied
        );
        assert!(
            store
                .get_node(loser.id())
                .await
                .unwrap()
                .unwrap()
                .is_archived()
        );
        assert!(
            store
                .get_edge(winner.id(), other.id())
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .get_edge(loser.id(), other.id())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .get_edge(scene.id(), winner.id())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            serde_json::to_value(store.get_edge(scene.id(), loser.id()).await.unwrap()).unwrap(),
            serde_json::to_value(Some(&evidence)).unwrap()
        );
        assert_episode_refusal(store.delete_node(loser.id()).await);
    }
}

impl CozoStore {
    pub(super) async fn put_node_for_import(
        &self,
        node: &Node,
        allow_touchstone_insert: bool,
    ) -> Result<()> {
        require_semantic_node(node, "put_node")?;
        if matches!(node.provenance(), Provenance::External { .. })
            && !matches!(
                self.read_meta(VECTOR_PROJECTION_META_KEY)?.as_deref(),
                Some(
                    SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER
                        | CONCERN_V1_CATALOG_GENERATION_MARKER
                        | EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER
                        | TOUCHSTONES_V1_CATALOG_GENERATION_MARKER
                )
            )
        {
            return Err(Error::InvalidInput(
                "external provenance requires a fresh capture-enabled store".into(),
            ));
        }
        let id = node.id().0.to_string();
        let data = encode_canonical_node(node)?;

        let mut p = BTreeMap::new();
        p.insert("id".into(), dv_str(&id));
        p.insert("data".into(), dv_str(&data));
        p.insert("status".into(), dv_str(status_str(node.status())));
        p.insert("summary".into(), dv_str(node.summary()));
        let mut scripts = vec![
            "?[id, data, status] <- [[$id, $data, $status]] :put node {id => data, status}"
                .to_owned(),
            "?[id, e, status] := *node_vec{id, e, status: old_status}, \
                id == $id, old_status != $status, status = $status \
                :put node_vec {id => e, status}"
                .to_owned(),
            "changed[id, summary, status] := id = $id, summary = $summary, \
                  status = $status, not *node_search{id}\n\
              changed[id, summary, status] := id = $id, summary = $summary, \
                  status = $status, *node_search{id, summary: old_summary}, \
                  old_summary != summary\n\
              changed[id, summary, status] := id = $id, summary = $summary, \
                  status = $status, *node_search{id, status: old_status}, \
                  old_status != status\n\
              ?[id, summary, status] := changed[id, summary, status] \
                :put node_search {id => summary, status}"
                .to_owned(),
        ];

        // Reconcile the exact physical tag set in the same transaction.  The
        // remove and put rules select only actual differences, so feedback and
        // other same-lane full-node rewrites perform zero physical tag writes.
        let tags: Vec<&str> = node.tags().collect();
        if tags.is_empty() {
            scripts.push(
                "?[tag, status, sample_hash, id] := \
                   *node_tag_v2:by_id{id, tag, status, sample_hash}, id == $id \
                   :rm node_tag_v2 {tag, status, sample_hash, id}"
                    .to_owned(),
            );
        } else {
            let mut rows = Vec::with_capacity(tags.len());
            for (index, tag) in tags.into_iter().enumerate() {
                p.insert(format!("tag{index}"), dv_str(tag));
                rows.push(format!("[$tag{index}, $status, $sample_hash, $id]"));
            }
            p.insert(
                "sample_hash".into(),
                dv_int(stable_tag_sample_hash(node.id())),
            );
            let desired = rows.join(", ");
            scripts.push(format!(
                "desired[tag, status, sample_hash, id] <- [{desired}]\n\
                 ?[tag, status, sample_hash, id] := \
                   *node_tag_v2:by_id{{id, tag, status, sample_hash}}, id == $id, \
                   not desired[tag, status, sample_hash, id] \
                   :rm node_tag_v2 {{tag, status, sample_hash, id}}"
            ));
            scripts.push(format!(
                "desired[tag, status, sample_hash, id] <- [{desired}]\n\
                 ?[tag, status, sample_hash, id] := \
                   desired[tag, status, sample_hash, id], \
                   not *node_tag_v2{{tag, status, sample_hash, id}} \
                   :put node_tag_v2 {{tag, status, sample_hash, id}}"
            ));
        }
        // The facet check and replacement share one snapshot. A caller cannot
        // flatten an immutable edition by submitting a semantic node at its ID.
        let tx = self.db.multi_transaction(true);
        let staged = (|| {
            tx_reject_episode_nodes(&tx, &[node.id()], "put_node")?;
            if matches!(node.provenance(), Provenance::External {source}
                if source.request_codec() == mneme_core::CaptureRequestCodec::TouchstoneV1)
                && (!touchstones::tx_generation(&tx)?
                    || (!allow_touchstone_insert
                        && touchstones::tx_record(&tx, node.id())?.is_none()))
            {
                return Err(Error::InvalidInput(
                    "touchstone owner requires atomic capture in touchstones-v1 generation".into(),
                ));
            }
            touchstones::tx_validate_owner_replacement(&tx, node)?;
            for script in &scripts {
                tx_run(&tx, script, p.clone())?;
            }
            Ok(())
        })();
        overlay::finish_transaction(&tx, staged)
    }
}

#[cfg(test)]
mod summary_edit_tests {
    use super::*;
    use mneme_core::{BodyRef, NodeSummary};

    fn fixture() -> Node {
        Node::try_new(
            NodeId(Ulid::new()),
            "oldsummary",
            BodyRef::new("inline://evidence").unwrap(),
            ["fixture"],
            Provenance::derived_empty(),
            0.3,
            0.7,
            NodeStatus::Archived,
            1,
        )
        .unwrap()
    }
    fn projections(store: &CozoStore, id: NodeId) -> Vec<String> {
        let tx = store.db.multi_transaction(false);
        let params = BTreeMap::from([("id".into(), dv_str(&id.0.to_string()))]);
        let rows = [
            "?[id,data,status] := *node{id,data,status}, id==$id",
            "?[id,e,status] := *node_vec{id,e,status}, id==$id",
            "?[id,summary,status] := *node_search{id,summary,status}, id==$id",
            "?[tag,status,sample_hash,id] := *node_tag_v2:by_id{id,tag,status,sample_hash}, id==$id",
        ].map(|query| format!("{:?}",tx_run(&tx,query,params.clone()).unwrap().rows)).to_vec();
        tx.commit().unwrap();
        rows
    }
    #[tokio::test]
    async fn summary_edit_rolls_back_every_transaction_stage_and_updates_all_projections() {
        let store = CozoStore::new(4).unwrap();
        let before = fixture();
        store.put_node(&before).await.unwrap();
        store
            .upsert(before.id(), &[1.0, 0.0, 0.0, 0.0])
            .await
            .unwrap();
        let expected = SummarySnapshot::from_node(store.db_id(), &before)
            .unwrap()
            .digest();
        let summary = NodeSummary::new("newsummary").unwrap();
        let original = projections(&store, before.id());
        for step in 1..=3 {
            assert!(
                summary_edit_failure_hook()
                    .lock()
                    .unwrap()
                    .insert(before.id(), step)
                    .is_none()
            );
            assert!(
                store
                    .compare_replace_node_summary(
                        before.id(),
                        &expected,
                        &summary,
                        &[0.0, 1.0, 0.0, 0.0]
                    )
                    .await
                    .is_err()
            );
            assert_eq!(
                projections(&store, before.id()),
                original,
                "partial commit at stage {step}"
            );
        }
        for embedding in [
            vec![1.0],
            vec![0.0; 4],
            vec![f32::NAN; 4],
            vec![f32::MAX; 4],
        ] {
            assert!(
                store
                    .compare_replace_node_summary(before.id(), &expected, &summary, &embedding)
                    .await
                    .is_err()
            );
            assert_eq!(projections(&store, before.id()), original);
        }
        let changed = store
            .compare_replace_node_summary(before.id(), &expected, &summary, &[0.0, 1.0, 0.0, 0.0])
            .await
            .unwrap();
        let after = projections(&store, before.id());
        assert_eq!(
            after[3], original[3],
            "summary curation must not rebuild or change tag projection"
        );
        for i in 0..3 {
            assert_ne!(after[i], original[i]);
        }
        let mut expected_node = before.clone();
        expected_node.set_summary(summary);
        assert_eq!(
            serde_json::to_value(changed).unwrap(),
            serde_json::to_value(expected_node).unwrap()
        );
        assert!(
            store
                .search("newsummary", 10, StatusFilter::ACTIVE)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .search("newsummary", 10, StatusFilter::ALL)
                .await
                .unwrap()[0]
                .id,
            before.id()
        );
        assert!(
            store
                .search("oldsummary", 10, StatusFilter::ALL)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn sqlite_summary_edit_rollback_and_commit_survive_reopen() {
        let dir = std::env::temp_dir().join(format!("mneme-summary-edit-{}", Ulid::new()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("graph.db");
        let path = path.to_str().unwrap();
        let node = fixture();
        let summary = NodeSummary::new("newsummary").unwrap();
        let (digest, original) = {
            let store = CozoStore::open(path, 4).unwrap();
            store.put_node(&node).await.unwrap();
            store
                .upsert(node.id(), &[1.0, 0.0, 0.0, 0.0])
                .await
                .unwrap();
            (
                SummarySnapshot::from_node(store.db_id(), &node)
                    .unwrap()
                    .digest(),
                projections(&store, node.id()),
            )
        };
        for step in 1..=3 {
            {
                let store =
                    super::super::tests::reopen_current_test_store(std::path::Path::new(path))
                        .unwrap();
                summary_edit_failure_hook()
                    .lock()
                    .unwrap()
                    .insert(node.id(), step);
                assert!(
                    store
                        .compare_replace_node_summary(
                            node.id(),
                            &digest,
                            &summary,
                            &[0.0, 1.0, 0.0, 0.0]
                        )
                        .await
                        .is_err()
                );
            }
            let store =
                super::super::tests::reopen_current_test_store(std::path::Path::new(path)).unwrap();
            assert_eq!(projections(&store, node.id()), original);
        }
        let committed = {
            let store =
                super::super::tests::reopen_current_test_store(std::path::Path::new(path)).unwrap();
            store
                .compare_replace_node_summary(node.id(), &digest, &summary, &[0.0, 1.0, 0.0, 0.0])
                .await
                .unwrap();
            projections(&store, node.id())
        };
        {
            let store =
                super::super::tests::reopen_current_test_store(std::path::Path::new(path)).unwrap();
            assert_eq!(projections(&store, node.id()), committed);
            assert_eq!(
                store.get_node(node.id()).await.unwrap().unwrap().summary(),
                "newsummary"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}

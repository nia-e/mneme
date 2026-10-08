//! Immutable source-authored episode editions and derived current-head indexes.
//!
//! The canonical node is the historical record. Every projection is constructed
//! inside the same transaction; replay observes the edition before head CAS.
use super::*;
use mneme_core::CaptureSource;
use mneme_core::episode::*;

#[cfg(test)]
mod read_tests;
mod reads;
#[cfg(test)]
mod tests;

const EPISODE_VECTOR_STATUS: &str = "episode";

#[async_trait]
impl EpisodeStore for CozoStore {
    async fn episode_header_by_edition(
        &self,
        request: &EpisodeHeaderByEditionRequest,
    ) -> Result<EpisodeHeaderByEdition> {
        self.episode_header_by_edition_impl(request).await
    }

    async fn lookup_episode(&self, source: &CaptureSource) -> Result<Option<EpisodeRecord>> {
        let source = source.clone();
        let dim = self.dim;
        self.episode_job(move |db| {
            let tx = db.multi_transaction(false);
            ensure_episode_generation(&tx)?;
            let result = match tx_node(&tx, source.node_id())? {
                None => None,
                Some(node) => {
                    require_source(&node, &source)?;
                    verify_edition(&tx, &node, dim)?;
                    Some(record(&tx, node)?)
                }
            };
            tx.commit().map_err(backend)?;
            Ok(result)
        })
        .await
    }

    async fn commit_episode(&self, request: EpisodeCommit<'_>) -> Result<EpisodeCommitOutcome> {
        let node = request.node;
        let source = episode_source(node)?.clone();
        if source.node_id() != node.id() {
            return Err(Error::InvalidInput(
                "episode source key does not match edition ID".into(),
            ));
        }
        mneme_core::ports::validate_capture_edges(node.id(), request.links)?;
        if request.embedding.len() != self.dim {
            return Err(Error::DimMismatch {
                index: self.dim,
                provider: request.embedding.len(),
            });
        }
        crate::validate_cosine_vector(request.embedding, "episode embedding")?;
        let canonical = encode_canonical_node(node)?;
        let node = node.clone();
        let embedding = request.embedding.to_vec();
        let links = request.links.to_vec();
        let expectation = request.expectation;
        let dim = self.dim;
        self.episode_job(move |db| {
            // The writer lock is acquired before source-key/head observations;
            // a lost acknowledgement retries the complete atomic transaction.
            let mut delay = std::time::Duration::from_millis(LOCK_RETRY_BASE_MS);
            for attempt in 0..=LOCK_RETRY_MAX {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let tx = db.multi_transaction(true);
                    let staged = stage_episode(
                        &tx,
                        &node,
                        &source,
                        &canonical,
                        &embedding,
                        &links,
                        expectation,
                        dim,
                    );
                    match staged {
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
                    Err(panic) => std::panic::resume_unwind(panic),
                }
                std::thread::sleep(delay);
                delay = (delay * 2).min(std::time::Duration::from_millis(LOCK_RETRY_CAP_MS));
            }
            unreachable!("bounded retry returns")
        })
        .await
    }

    async fn get_episode(&self, request: &EpisodeGet) -> Result<Option<EpisodeRecord>> {
        self.episode_get_impl(request).await
    }
    async fn episode_timeline(
        &self,
        request: &EpisodeTimelineRequest,
    ) -> Result<EpisodePage<EpisodeTimelineCursor>> {
        self.episode_timeline_impl(request).await
    }
    async fn episode_cue(&self, request: &EpisodeCueRequest) -> Result<EpisodeCuePage> {
        self.episode_cue_impl(request).await
    }
    async fn episode_history(
        &self,
        request: &EpisodeHistoryRequest,
    ) -> Result<EpisodePage<EpisodeHistoryCursor>> {
        self.episode_history_impl(request).await
    }
    async fn episode_references(
        &self,
        request: &EpisodeReferencesRequest,
    ) -> Result<EpisodeReferencesPage> {
        self.episode_references_impl(request).await
    }
}

impl CozoStore {
    pub(super) async fn episode_job<T: Send + 'static>(
        &self,
        action: impl FnOnce(&DbInstance) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let job = ActiveBackendJob {
            db: self.db.clone(),
            authority: self.persistent_authority.clone(),
            activity: self.backend_activity.enter(),
        };
        tokio::task::spawn_blocking(move || job.run(action))
            .await
            .map_err(|error| backend_str(format!("join episode worker: {error}")))?
    }
}

pub(super) fn ensure_episode_generation(tx: &MultiTransaction) -> Result<()> {
    let mut p = BTreeMap::new();
    p.insert("key".into(), dv_str(VECTOR_PROJECTION_META_KEY));
    let rows = tx_run(tx, "?[v] := *meta{k: $key, v}", p)?;
    match rows.rows.as_slice() {
        [row]
            if matches!(
                want_str(&row[0])?,
                SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER
                    | CONCERN_V1_CATALOG_GENERATION_MARKER
                    | EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER
                    | TOUCHSTONES_V1_CATALOG_GENERATION_MARKER
            ) =>
        {
            Ok(())
        }
        _ => Err(Error::InvalidInput(
            "episode generation required; explicitly upgrade this store".into(),
        )),
    }
}

pub(super) fn tx_node(tx: &MultiTransaction, id: NodeId) -> Result<Option<Node>> {
    let mut p = BTreeMap::new();
    p.insert("id".into(), dv_str(&id.0.to_string()));
    tx_run(
        tx,
        "?[id, data, status] := *node{id: $id, data, status}, id = $id",
        p,
    )?
    .rows
    .first()
    .map(|row| decode_canonical_node_row(row))
    .transpose()
}

pub(super) fn tx_head(
    tx: &MultiTransaction,
    root: EpisodeId,
) -> Result<Option<(NodeId, u32, Timestamp)>> {
    let mut p = BTreeMap::new();
    p.insert("root".into(), dv_str(&root.get().0.to_string()));
    let rows = tx_run(
        tx,
        "?[head, revision, recorded_at] := *episode_head{root: $root, head, revision, recorded_at}",
        p,
    )?;
    rows.rows
        .first()
        .map(|r| {
            Ok((
                node_id(want_str(&r[0])?)?,
                u32::try_from(want_i64(&r[1])?)
                    .map_err(|_| backend_str("invalid episode head revision".into()))?,
                Timestamp::try_from(want_i64(&r[2])?)
                    .map_err(|_| backend_str("invalid episode recording time".into()))?,
            ))
        })
        .transpose()
}

fn episode_source(node: &Node) -> Result<&CaptureSource> {
    if node.episode().is_none() {
        return Err(Error::InvalidInput(
            "episode operation requires an episode edition".into(),
        ));
    }
    match node.provenance() {
        Provenance::External { source } => Ok(source),
        _ => Err(Error::InvalidInput(
            "episode requires source-authored provenance".into(),
        )),
    }
}
fn require_source(node: &Node, source: &CaptureSource) -> Result<()> {
    match episode_source(node) {
        Ok(stored) if stored == source => Ok(()),
        _ => Err(Error::Conflict(
            "episode source key collides with an existing record".into(),
        )),
    }
}

fn identity(node: &Node) -> Result<EpisodeIdentity> {
    let facet = node
        .episode()
        .ok_or_else(|| backend_str("episode index refers to a semantic node".into()))?;
    Ok(EpisodeIdentity {
        episode_id: facet.root(),
        edition_id: node.id(),
        revision: facet.revision(),
    })
}

pub(super) fn record(tx: &MultiTransaction, node: Node) -> Result<EpisodeRecord> {
    let identity = identity(&node)?;
    let (current_edition_id, _, _) = tx_head(tx, identity.episode_id)?
        .ok_or_else(|| backend_str("episode edition has no current head".into()))?;
    Ok(EpisodeRecord {
        identity,
        node,
        current_edition_id,
    })
}

pub(super) fn header(node: &Node, current_edition_id: NodeId) -> Result<EpisodeHeader> {
    EpisodeHeader::from_node(node, current_edition_id)
        .map_err(|error| Error::InvalidInput(error.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn stage_episode(
    tx: &MultiTransaction,
    node: &Node,
    source: &CaptureSource,
    canonical: &str,
    embedding: &[f32],
    links: &[Edge],
    expectation: EpisodeWriteExpectation,
    dim: usize,
) -> Result<EpisodeCommitOutcome> {
    ensure_episode_generation(tx)?;
    if node
        .episode()
        .is_some_and(|facet| facet.occurrence_contexts().is_some())
    {
        let rows = tx_run(
            tx,
            "?[v] := *meta{k: $key, v}",
            BTreeMap::from([("key".into(), dv_str(VECTOR_PROJECTION_META_KEY))]),
        )?;
        if rows.rows.len() != 1
            || !matches!(
                want_str(&rows.rows[0][0])?,
                EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER
                    | TOUCHSTONES_V1_CATALOG_GENERATION_MARKER
            )
        {
            return Err(Error::InvalidInput(
                "occurrence contexts require episode-context-v2 generation".into(),
            ));
        }
    }
    tx_run(
        tx,
        "?[k, v] := *meta{k, v}, k == 'db_id' :put meta {k => v}",
        BTreeMap::new(),
    )?;
    ensure_episode_generation(tx)?;
    if let Some(existing) = tx_node(tx, node.id())? {
        require_source(&existing, source)?;
        verify_edition(tx, &existing, dim)?;
        return Ok(EpisodeCommitOutcome::AlreadyApplied(identity(&existing)?));
    }
    let facet = node.episode().expect("prevalidated facet");
    // Absence of the canonical node is not permission to heal an incomplete
    // capture. Refuse named residual projections before any row is overwritten.
    let mut orphan = projection_params(node)?;
    orphan.insert("id".into(), dv_str(&node.id().0.to_string()));
    let rows = tx_run(
        tx,
        "orphan[id] := *node_vec{id}, id == $id\norphan[id] := *node_search{id}, id == $id\norphan[id] := *node_tag_v2:by_id{id}, id == $id\norphan[id] := *episode_history{root: $root, revision: $revision, edition: id}, id == $id\norphan[id] := *edge{from: id}, id == $id\norphan[id] := *edge:by_to{to: id}, id == $id\n?[id] := orphan[id] :limit 1",
        orphan,
    )?;
    if !rows.rows.is_empty() {
        return Err(Error::Conflict(
            "episode source identity has incomplete existing projections".into(),
        ));
    }
    let old_head = tx_head(tx, facet.root())?;
    match expectation {
        EpisodeWriteExpectation::NewRoot => {
            if old_head.is_some()
                || facet.root().get() != node.id()
                || facet.revises().is_some()
                || facet.revision().get() != 0
            {
                return Err(Error::Conflict(
                    "episode root already exists or initial edition is invalid".into(),
                ));
            }
        }
        EpisodeWriteExpectation::CurrentEdition(expected) => {
            let Some((current, revision, recorded_at)) = old_head else {
                return Err(Error::NotFound);
            };
            if current != expected
                || facet.revises() != Some(expected)
                || revision.checked_add(1) != Some(facet.revision().get())
                || facet.recorded_at().get() != recorded_at
            {
                return Err(Error::Conflict(
                    "episode current head changed; editorial revision was not rebased".into(),
                ));
            }
            let predecessor = tx_node(tx, expected)?.ok_or(Error::NotFound)?;
            if predecessor.episode().is_none_or(|prior| {
                prior.root() != facet.root() || prior.revision().get() != revision
            }) {
                return Err(backend_str(
                    "episode current head is inconsistent with its edition".into(),
                ));
            }
        }
    }
    capture::validate_authored_links(tx, node.id(), links)?;
    let mut p = BTreeMap::new();
    p.insert("id".into(), dv_str(&node.id().0.to_string()));
    p.insert("data".into(), dv_str(canonical));
    p.insert("status".into(), dv_str(status_str(node.status())));
    tx_run(
        tx,
        "?[id, data, status] <- [[$id, $data, $status]] :put node {id => data, status}",
        p.clone(),
    )?;
    fail_episode(node.id(), 1)?;
    p.insert("e".into(), dv_float_list(embedding));
    p.insert("status".into(), dv_str(EPISODE_VECTOR_STATUS));
    tx_run(
        tx,
        "?[id, e, status] := id = $id, e = vec($e), status = $status :put node_vec {id => e, status}",
        p,
    )?;
    fail_episode(node.id(), 2)?;
    capture::write_authored_links(tx, node.id(), links)?;
    fail_episode(node.id(), 3)?;
    if let Some((old, _, _)) = old_head {
        let old_node = tx_node(tx, old)?.ok_or(Error::NotFound)?;
        remove_head_projection(tx, &old_node)?;
    }
    write_edition_projection(tx, node)?;
    fail_episode(node.id(), 4)?;
    write_head_projection(tx, node)?;
    fail_episode(node.id(), 5)?;
    verify_edition(tx, node, dim)?;
    Ok(EpisodeCommitOutcome::Applied(identity(node)?))
}

fn occurrence_bounds(facet: &EpisodeFacet) -> (Option<Timestamp>, Option<Timestamp>) {
    match facet.occurrence() {
        OccurrenceSpan::Unknown => (None, None),
        OccurrenceSpan::Point { at } => (Some(at.get()), Some(at.get())),
        OccurrenceSpan::Range { start, end } => (Some(start.get()), Some(end.get())),
    }
}
fn projection_params(node: &Node) -> Result<BTreeMap<String, DataValue>> {
    let facet = node
        .episode()
        .ok_or_else(|| Error::InvalidInput("episode facet missing".into()))?;
    let mut p = BTreeMap::new();
    p.insert("root".into(), dv_str(&facet.root().get().0.to_string()));
    p.insert("head".into(), dv_str(&node.id().0.to_string()));
    p.insert("revision".into(), dv_int(i64::from(facet.revision().get())));
    p.insert(
        "recorded_at".into(),
        dv_int(facet.recorded_at().get() as i64),
    );
    p.insert("summary".into(), dv_str(node.summary()));
    p.insert(
        "thread".into(),
        dv_str(facet.thread().map_or("", |thread| thread.as_str())),
    );
    let (start, end) = occurrence_bounds(facet);
    p.insert(
        "occurred_start".into(),
        start.map_or(DataValue::Null, |t| dv_int(t as i64)),
    );
    p.insert(
        "occurred_end".into(),
        end.map_or(DataValue::Null, |t| dv_int(t as i64)),
    );
    Ok(p)
}
fn time_keys(node: &Node) -> Vec<(&'static str, String, Timestamp)> {
    let facet = node.episode().expect("validated episode");
    let mut threads = vec![String::new()];
    if let Some(thread) = facet.thread() {
        threads.push(thread.as_str().to_owned());
    }
    let mut keys = Vec::new();
    for thread in threads {
        keys.push(("recorded", thread.clone(), facet.recorded_at().get()));
        if let Some(start) = occurrence_bounds(facet).0 {
            keys.push(("occurred", thread, start));
        }
    }
    keys
}
fn write_edition_projection(tx: &MultiTransaction, node: &Node) -> Result<()> {
    tx_run(
        tx,
        "?[root, revision, edition] <- [[$root, $revision, $head]] :put episode_history {root, revision => edition}",
        projection_params(node)?,
    )?;
    Ok(())
}
fn write_head_projection(tx: &MultiTransaction, node: &Node) -> Result<()> {
    let p = projection_params(node)?;
    tx_run(
        tx,
        "?[root, head, revision, recorded_at] <- [[$root, $head, $revision, $recorded_at]] :put episode_head {root => head, revision, recorded_at}",
        p.clone(),
    )?;
    tx_run(
        tx,
        "?[root, head, summary, thread, recorded_at, occurred_start, occurred_end] <- [[$root, $head, $summary, $thread, $recorded_at, $occurred_start, $occurred_end]] :put episode_search {root => head, summary, thread, recorded_at, occurred_start, occurred_end}",
        p.clone(),
    )?;
    for (axis, thread, at) in time_keys(node) {
        let mut p = p.clone();
        p.insert("axis".into(), dv_str(axis));
        p.insert("thread".into(), dv_str(&thread));
        p.insert("at".into(), dv_int(at as i64));
        tx_run(
            tx,
            "?[axis, thread, at, root, head, occurred_start, occurred_end] <- [[$axis, $thread, $at, $root, $head, $occurred_start, $occurred_end]] :put episode_time {axis, thread, at, root => head, occurred_start, occurred_end}",
            p,
        )?;
    }
    Ok(())
}
fn remove_head_projection(tx: &MultiTransaction, node: &Node) -> Result<()> {
    let p = projection_params(node)?;
    for (axis, thread, at) in time_keys(node) {
        let mut p = p.clone();
        p.insert("axis".into(), dv_str(axis));
        p.insert("thread".into(), dv_str(&thread));
        p.insert("at".into(), dv_int(at as i64));
        tx_run(
            tx,
            "?[axis, thread, at, root] <- [[$axis, $thread, $at, $root]] :rm episode_time {axis, thread, at, root}",
            p,
        )?;
    }
    Ok(())
}
fn verify_edition(tx: &MultiTransaction, node: &Node, dim: usize) -> Result<()> {
    let facet = node
        .episode()
        .ok_or_else(|| backend_str("episode facet missing".into()))?;
    let mut p = projection_params(node)?;
    p.insert("id".into(), dv_str(&node.id().0.to_string()));
    let rows = tx_run(
        tx,
        "?[e, status] := *node_vec{id: $id, e, status}",
        p.clone(),
    )?;
    let row = rows
        .rows
        .first()
        .ok_or_else(|| backend_str("episode vector projection missing".into()))?;
    let embedding = want_vector(&row[0])?;
    if embedding.len() != dim || want_str(&row[1])? != EPISODE_VECTOR_STATUS {
        return Err(backend_str("episode vector projection inconsistent".into()));
    }
    crate::validate_cosine_vector(&embedding, "stored episode embedding")?;
    let rows = tx_run(
        tx,
        "?[edition] := *episode_history{root: $root, revision: $revision, edition}",
        p.clone(),
    )?;
    if rows.rows.len() != 1 || node_id(want_str(&rows.rows[0][0])?)? != node.id() {
        return Err(backend_str(
            "episode history projection inconsistent".into(),
        ));
    }
    let (head, revision, recorded_at) =
        tx_head(tx, facet.root())?.ok_or_else(|| backend_str("episode head missing".into()))?;
    if revision < facet.revision().get() || recorded_at != facet.recorded_at().get() {
        return Err(backend_str("episode head projection inconsistent".into()));
    }
    let semantic = tx_run(
        tx,
        "?[id] := *node_search{id: $id}, id = $id\n?[id] := *node_tag_v2:by_id{id: $id}, id = $id :limit 1",
        p.clone(),
    )?;
    if !semantic.rows.is_empty() {
        return Err(backend_str(
            "episode leaked into semantic projection".into(),
        ));
    }
    if head == node.id() {
        let search = tx_run(
            tx,
            "?[head, summary, thread, recorded_at, occurred_start, occurred_end] := *episode_search{root: $root, head, summary, thread, recorded_at, occurred_start, occurred_end}",
            p.clone(),
        )?;
        let expected = [
            p["head"].clone(),
            p["summary"].clone(),
            p["thread"].clone(),
            p["recorded_at"].clone(),
            p["occurred_start"].clone(),
            p["occurred_end"].clone(),
        ];
        if search.rows.as_slice() != [expected.to_vec()] {
            return Err(backend_str(
                "episode lexical head projection inconsistent".into(),
            ));
        }
        for (axis, thread, at) in time_keys(node) {
            let mut p = p.clone();
            p.insert("axis".into(), dv_str(axis));
            p.insert("thread".into(), dv_str(&thread));
            p.insert("at".into(), dv_int(at as i64));
            let row = tx_run(
                tx,
                "?[head, occurred_start, occurred_end] := *episode_time{axis: $axis, thread: $thread, at: $at, root: $root, head, occurred_start, occurred_end}",
                p.clone(),
            )?;
            let expected = vec![
                p["head"].clone(),
                p["occurred_start"].clone(),
                p["occurred_end"].clone(),
            ];
            if row.rows.as_slice() != [expected] {
                return Err(backend_str("episode time projection inconsistent".into()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn failures() -> &'static std::sync::Mutex<BTreeMap<NodeId, usize>> {
    static STATE: std::sync::OnceLock<std::sync::Mutex<BTreeMap<NodeId, usize>>> =
        std::sync::OnceLock::new();
    STATE.get_or_init(Default::default)
}
#[cfg(test)]
fn fail_episode(id: NodeId, step: usize) -> Result<()> {
    let mut failures = failures().lock().unwrap();
    if failures.get(&id) == Some(&step) {
        failures.remove(&id);
        return Err(backend_str(format!("injected episode failure {step}")));
    }
    Ok(())
}
#[cfg(not(test))]
fn fail_episode(_: NodeId, _: usize) -> Result<()> {
    Ok(())
}

impl CozoStore {
    /// Detached import only: caller validated the complete canonical export and
    /// proved the target empty. Generic put_node intentionally cannot do this.
    pub(super) fn import_episode_editions(&self, export: &crate::StoreExport) -> Result<()> {
        crate::validate_episode_import(export)?;
        let nodes: Vec<_> = export
            .nodes
            .iter()
            .filter(|node| node.episode().is_some())
            .collect();
        if nodes.is_empty() {
            return Ok(());
        }
        let vectors: HashMap<_, _> = export.vectors.iter().map(|(id, v)| (*id, v)).collect();
        let tx = self.db.multi_transaction(true);
        let result = (|| {
            ensure_episode_generation(&tx)?;
            let mut heads: BTreeMap<EpisodeId, &Node> = BTreeMap::new();
            for node in nodes {
                let canonical = encode_canonical_node(node)?;
                let vector = vectors
                    .get(&node.id())
                    .ok_or_else(|| Error::InvalidInput("episode import vector missing".into()))?;
                let mut p = BTreeMap::new();
                p.insert("id".into(), dv_str(&node.id().0.to_string()));
                p.insert("data".into(), dv_str(&canonical));
                p.insert("status".into(), dv_str(status_str(node.status())));
                tx_run(
                    &tx,
                    "?[id, data, status] <- [[$id,$data,$status]] :put node {id=>data,status}",
                    p.clone(),
                )?;
                p.insert("e".into(), dv_float_list(vector));
                p.insert("status".into(), dv_str(EPISODE_VECTOR_STATUS));
                tx_run(
                    &tx,
                    "?[id,e,status] := id=$id,e=vec($e),status=$status :put node_vec {id=>e,status}",
                    p,
                )?;
                write_edition_projection(&tx, node)?;
                let facet = node.episode().expect("filtered");
                let replace = heads.get(&facet.root()).is_none_or(|head| {
                    head.episode().unwrap().revision().get() < facet.revision().get()
                });
                if replace {
                    heads.insert(facet.root(), node);
                }
            }
            for head in heads.into_values() {
                write_head_projection(&tx, head)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => tx.commit().map_err(backend),
            Err(error) => {
                let _ = tx.abort();
                Err(error)
            }
        }
    }

    /// Cold construction verification derives every episode projection from the
    /// canonical export; equality also rejects stale/orphaned head/time rows.
    pub(super) fn verify_episode_import(&self, export: &crate::StoreExport) -> Result<()> {
        crate::validate_episode_import(export)?;
        if !matches!(
            self.read_meta(VECTOR_PROJECTION_META_KEY)?.as_deref(),
            Some(
                SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER
                    | CONCERN_V1_CATALOG_GENERATION_MARKER
                    | EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER
                    | TOUCHSTONES_V1_CATALOG_GENERATION_MARKER
            )
        ) {
            if export.nodes.iter().any(|node| !node.is_semantic()) {
                return Err(Error::InvalidInput(
                    "episode import requires episode generation".into(),
                ));
            }
            return Ok(());
        }
        let tx = self.db.multi_transaction(false);
        ensure_episode_generation(&tx)?;
        let mut heads: BTreeMap<EpisodeId, &Node> = BTreeMap::new();
        let mut histories = BTreeSet::new();
        for node in export.nodes.iter().filter(|node| node.episode().is_some()) {
            let facet = node.episode().unwrap();
            verify_edition(&tx, node, self.dim)?;
            histories.insert(vec![
                dv_str(&facet.root().get().0.to_string()),
                dv_int(i64::from(facet.revision().get())),
                dv_str(&node.id().0.to_string()),
            ]);
            if heads.get(&facet.root()).is_none_or(|head| {
                head.episode().unwrap().revision().get() < facet.revision().get()
            }) {
                heads.insert(facet.root(), node);
            }
        }
        let mut head_rows = BTreeSet::new();
        let mut time_rows = BTreeSet::new();
        let mut search_rows = BTreeSet::new();
        for node in heads.into_values() {
            let p = projection_params(node)?;
            head_rows.insert(vec![
                p["root"].clone(),
                p["head"].clone(),
                p["revision"].clone(),
                p["recorded_at"].clone(),
            ]);
            search_rows.insert(vec![
                p["root"].clone(),
                p["head"].clone(),
                p["summary"].clone(),
                p["thread"].clone(),
                p["recorded_at"].clone(),
                p["occurred_start"].clone(),
                p["occurred_end"].clone(),
            ]);
            for (axis, thread, at) in time_keys(node) {
                time_rows.insert(vec![
                    dv_str(axis),
                    dv_str(&thread),
                    dv_int(at as i64),
                    p["root"].clone(),
                    p["head"].clone(),
                    p["occurred_start"].clone(),
                    p["occurred_end"].clone(),
                ]);
            }
        }
        for (query, expected) in [
            (
                "?[root,head,revision,recorded_at] := *episode_head{root,head,revision,recorded_at}",
                head_rows,
            ),
            (
                "?[root,revision,edition] := *episode_history{root,revision,edition}",
                histories,
            ),
            (
                "?[axis,thread,at,root,head,occurred_start,occurred_end] := *episode_time{axis,thread,at,root,head,occurred_start,occurred_end}",
                time_rows,
            ),
            (
                "?[root,head,summary,thread,recorded_at,occurred_start,occurred_end] := *episode_search{root,head,summary,thread,recorded_at,occurred_start,occurred_end}",
                search_rows,
            ),
        ] {
            let actual: BTreeSet<_> = tx_run(&tx, query, BTreeMap::new())?
                .rows
                .into_iter()
                .collect();
            if actual != expected {
                return Err(backend_str(
                    "episode derived inventory differs from canonical export".into(),
                ));
            }
        }
        tx.commit().map_err(backend)
    }
}

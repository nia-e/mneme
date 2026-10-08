//! Reference episodic storage. Canonical editions are immutable; all lookup
//! structures here are disposable projections rebuilt from those editions.

use super::*;

pub(super) fn require_semantic_node(node: &Node) -> Result<()> {
    if node.is_semantic() {
        Ok(())
    } else {
        Err(Error::InvalidInput(
            "episode editions are immutable; use the dedicated episode operation".into(),
        ))
    }
}

impl Inner {
    pub(super) fn require_semantic_ids(&self, ids: &[NodeId]) -> Result<()> {
        for id in ids {
            if let Some(node) = self.nodes.get(id) {
                require_semantic_node(node)?;
            }
        }
        Ok(())
    }

    pub(super) fn is_episode_edge(&self, edge: &Edge) -> bool {
        [edge.from, edge.to]
            .iter()
            .any(|id| self.nodes.get(id).is_some_and(|node| !node.is_semantic()))
    }

    pub(super) fn require_no_episode_evidence(&self, id: NodeId) -> Result<()> {
        if self
            .incident_edges(id)
            .iter()
            .any(|edge| self.is_episode_edge(edge))
        {
            return Err(Error::InvalidInput(
                "memory is an episode evidence anchor; archive it rather than forgetting it".into(),
            ));
        }
        Ok(())
    }
}

use mneme_core::episode::*;

/// All authoritative episode information lives in canonical nodes. These
/// indexes contain IDs/keys only and cannot manufacture a missing edition.
#[derive(Default)]
pub(super) struct EpisodeProjection {
    heads: BTreeMap<EpisodeId, NodeId>,
    history: BTreeMap<EpisodeId, BTreeMap<u32, NodeId>>,
    recorded: BTreeMap<Option<String>, BTreeSet<(EpisodeTime, EpisodeId)>>,
    occurred: BTreeMap<Option<String>, BTreeSet<(EpisodeTime, EpisodeId)>>,
    lexical: BTreeMap<String, BTreeSet<EpisodeId>>,
}

impl EpisodeProjection {
    pub(super) fn rebuild(nodes: &BTreeMap<NodeId, Node>) -> Result<Self> {
        let mut projection = Self::default();
        for node in nodes.values() {
            let Some(facet) = node.episode() else {
                continue;
            };
            node.validate()
                .map_err(|error| Error::InvalidInput(error.to_string()))?;
            let history = projection.history.entry(facet.root()).or_default();
            if history.insert(facet.revision().get(), node.id()).is_some() {
                return Err(Error::InvalidInput(
                    "episode import contains competing editions".into(),
                ));
            }
        }
        for (&root, history) in &projection.history {
            let mut previous: Option<&Node> = None;
            for (ordinal, (&revision, id)) in history.iter().enumerate() {
                if usize::try_from(revision).ok() != Some(ordinal) {
                    return Err(Error::InvalidInput(
                        "episode import has a missing revision".into(),
                    ));
                }
                let node = &nodes[id];
                let facet = node.episode().expect("episode history index");
                if let Some(previous) = previous {
                    facet.validate_successor(previous)?;
                } else if facet.revises().is_some() || root.node_id() != node.id() {
                    return Err(Error::InvalidInput(
                        "episode import is missing its original root".into(),
                    ));
                }
                previous = Some(node);
            }
            projection
                .heads
                .insert(root, previous.expect("nonempty episode history").id());
        }
        // Build current-head projections only after the entire chain validates.
        let heads: Vec<_> = projection.heads.values().copied().collect();
        for id in heads {
            projection.index_head(&nodes[&id]);
        }
        Ok(projection)
    }

    fn index_head(&mut self, node: &Node) {
        let facet = node.episode().expect("episode head");
        let mut buckets = vec![None];
        if let Some(thread) = facet.thread() {
            buckets.push(Some(thread.as_str().to_owned()));
        }
        for bucket in buckets {
            self.recorded
                .entry(bucket.clone())
                .or_default()
                .insert((facet.recorded_at(), facet.root()));
            if let Some(at) = facet.occurrence().start() {
                self.occurred
                    .entry(bucket)
                    .or_default()
                    .insert((at, facet.root()));
            }
        }
        for term in lexical_terms(node.summary()) {
            self.lexical.entry(term).or_default().insert(facet.root());
        }
    }

    fn unindex_head(&mut self, node: &Node) {
        let facet = node.episode().expect("episode head");
        let mut buckets = vec![None];
        if let Some(thread) = facet.thread() {
            buckets.push(Some(thread.as_str().to_owned()));
        }
        for bucket in buckets {
            if let Some(keys) = self.recorded.get_mut(&bucket) {
                keys.remove(&(facet.recorded_at(), facet.root()));
            }
            if let Some(at) = facet.occurrence().start()
                && let Some(keys) = self.occurred.get_mut(&bucket)
            {
                keys.remove(&(at, facet.root()));
            }
        }
        for term in lexical_terms(node.summary()) {
            if let Some(roots) = self.lexical.get_mut(&term) {
                roots.remove(&facet.root());
            }
        }
    }

    fn publish(&mut self, node: &Node, previous: Option<&Node>) {
        if let Some(previous) = previous {
            self.unindex_head(previous);
        }
        let facet = node.episode().expect("validated episode publication");
        self.history
            .entry(facet.root())
            .or_default()
            .insert(facet.revision().get(), node.id());
        self.heads.insert(facet.root(), node.id());
        self.index_head(node);
    }
}

/// Shared pre-publication validation for reference and persistent imports.
/// Legacy semantic JSON retains its existing compatibility policy; actual
/// episode editions require complete, unambiguous chains and canonical vectors.
pub(crate) fn validate_episode_import(export: &StoreExport) -> Result<()> {
    let mut nodes = BTreeMap::new();
    for node in &export.nodes {
        if let Some(previous) = nodes.insert(node.id(), node.clone())
            && (!node.is_semantic() || !previous.is_semantic())
        {
            return Err(Error::InvalidInput(
                "episode import contains duplicate edition identity".into(),
            ));
        }
    }
    EpisodeProjection::rebuild(&nodes)?;
    let mut vectors = HashMap::new();
    for (id, vector) in &export.vectors {
        if nodes.get(id).is_some_and(|node| !node.is_semantic()) {
            if vectors.insert(*id, vector).is_some() {
                return Err(Error::InvalidInput(
                    "episode import contains duplicate vectors".into(),
                ));
            }
            if vector.len() != export.dim {
                return Err(Error::DimMismatch {
                    index: export.dim,
                    provider: vector.len(),
                });
            }
            validate_cosine_vector(vector, "episode import vector")?;
        }
    }
    for node in nodes.values().filter(|node| !node.is_semantic()) {
        if !vectors.contains_key(&node.id()) {
            return Err(Error::InvalidInput(
                "episode import has an edition without a vector".into(),
            ));
        }
    }
    // Episode editions never participate in semantic rewrite ledgers, even a
    // closed imported one: these proofs authorize later idempotent responses.
    for pair in export
        .contradictions
        .iter()
        .map(|x| x.between)
        .chain(export.merges.iter().map(|x| x.between))
        .chain(export.full_merge_commits.iter().map(|x| x.between))
        .chain(export.supersede_commits.iter().map(|x| x.between))
    {
        if [pair.0, pair.1]
            .iter()
            .any(|id| nodes.get(id).is_some_and(|node| !node.is_semantic()))
        {
            return Err(Error::InvalidInput(
                "episode import contains semantic rewrite evidence".into(),
            ));
        }
    }
    Ok(())
}

fn identity(node: &Node) -> EpisodeIdentity {
    node.episode()
        .expect("validated episode")
        .identity(node.id())
}

fn record(inner: &Inner, node: &Node) -> Result<EpisodeRecord> {
    let identity = identity(node);
    let current_edition_id = *inner
        .episode_projection
        .heads
        .get(&identity.episode_id)
        .ok_or_else(|| Error::Conflict("episode has no current head".into()))?;
    Ok(EpisodeRecord {
        identity,
        node: node.clone(),
        current_edition_id,
    })
}

fn lookup(inner: &Inner, source: &CaptureSource) -> Result<Option<EpisodeRecord>> {
    source
        .validate()
        .map_err(|e| Error::InvalidInput(e.to_string()))?;
    let id = source.node_id();
    match inner.nodes.get(&id) {
        Some(node) => match node.provenance() {
            Provenance::External { source: stored }
                if stored == source
                    && node.episode().is_some()
                    && inner.vectors.contains_key(&id) =>
            {
                record(inner, node).map(Some)
            }
            _ => Err(Error::Conflict(
                "episode source identity contains different or incomplete content".into(),
            )),
        },
        None if inner.vectors.contains_key(&id) => Err(Error::Conflict(
            "episode source identity has an orphan vector".into(),
        )),
        None => Ok(None),
    }
}

#[async_trait]
impl mneme_core::ports::EpisodeStore for MemStore {
    async fn episode_header_by_edition(
        &self,
        request: &EpisodeHeaderByEditionRequest,
    ) -> Result<EpisodeHeaderByEdition> {
        request.validate()?;
        let started = std::time::Instant::now();
        let inner = self.linked_read_lock(started, request.remaining())?;
        let result = match inner.nodes.get(&request.edition_id()) {
            None => EpisodeHeaderByEdition::Missing,
            Some(node) => match node.episode() {
                None => EpisodeHeaderByEdition::Semantic,
                Some(facet) => {
                    let head = inner
                        .episode_projection
                        .heads
                        .get(&facet.root())
                        .ok_or_else(|| {
                            Error::Backend("episode edition has no current head".into())
                        })?;
                    EpisodeHeaderByEdition::Episode(header(node, *head)?)
                }
            },
        };
        linked_read_remaining(started, request.remaining())?;
        Ok(result)
    }

    async fn lookup_episode(&self, source: &CaptureSource) -> Result<Option<EpisodeRecord>> {
        lookup(&self.lock(), source)
    }

    async fn commit_episode(&self, request: EpisodeCommit<'_>) -> Result<EpisodeCommitOutcome> {
        let node = request.node;
        node.validate()
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        let facet = node.episode().ok_or_else(|| {
            Error::InvalidInput("episode commit requires an episode facet".into())
        })?;
        let Provenance::External { source } = node.provenance() else {
            return Err(Error::InvalidInput(
                "episode commit requires source provenance".into(),
            ));
        };
        if source.node_id() != node.id() {
            return Err(Error::InvalidInput(
                "episode source does not identify this edition".into(),
            ));
        }
        match request.expectation {
            EpisodeWriteExpectation::NewRoot if facet.revision() != EpisodeRevision::INITIAL => {
                return Err(Error::InvalidInput(
                    "new episode request has a revision facet".into(),
                ));
            }
            EpisodeWriteExpectation::CurrentEdition(expected)
                if facet.revises() != Some(expected) =>
            {
                return Err(Error::InvalidInput(
                    "episode request predecessor differs from its facet".into(),
                ));
            }
            _ => {}
        }
        mneme_core::ports::validate_capture_edges(node.id(), request.links)?;
        if request.embedding.len() != self.dim {
            return Err(Error::DimMismatch {
                index: self.dim,
                provider: request.embedding.len(),
            });
        }
        validate_cosine_vector(request.embedding, "episode embedding")?;
        let mut inner = self.lock();
        // Immutable source proof wins before head CAS. A response lost before
        // later edits remains an exact replay of the original edition.
        if let Some(existing) = lookup(&inner, source)? {
            return Ok(EpisodeCommitOutcome::AlreadyApplied(existing.identity));
        }
        let previous = match request.expectation {
            EpisodeWriteExpectation::NewRoot => {
                if facet.revision().get() != 0
                    || facet.root().node_id() != node.id()
                    || inner.episode_projection.heads.contains_key(&facet.root())
                {
                    return Err(Error::Conflict(
                        "episode root already exists or new-root facet is invalid".into(),
                    ));
                }
                None
            }
            EpisodeWriteExpectation::CurrentEdition(expected) => {
                if inner.episode_projection.heads.get(&facet.root()) != Some(&expected) {
                    return Err(Error::Conflict(
                        "episode head changed; reread before revising".into(),
                    ));
                }
                let previous = inner
                    .nodes
                    .get(&expected)
                    .ok_or_else(|| Error::Conflict("episode expected edition is missing".into()))?;
                facet.validate_successor(previous)?;
                Some(previous.clone())
            }
        };
        if inner
            .edge_keys_by_endpoint
            .get(&node.id())
            .is_some_and(|keys| !keys.is_empty())
            || inner
                .tag_projection
                .contains_sample((stable_tag_sample_hash(node.id()), node.id()))
        {
            return Err(Error::Conflict(
                "episode identity has orphan graph projections".into(),
            ));
        }
        for edge in request.links {
            if !inner.nodes.contains_key(&edge.to) {
                return Err(Error::InvalidInput(
                    "episode link target does not exist".into(),
                ));
            }
            inner.validate_edge_upsert_capacity((edge.from, edge.to))?;
        }
        let next_epoch = inner.preflight_epoch_advance(true)?;
        inner.episode_projection.publish(node, previous.as_ref());
        inner.replace_node(node.clone());
        inner.vectors.insert(node.id(), request.embedding.to_vec());
        for edge in request.links {
            inner.put_edge_prevalidated(edge.clone());
        }
        inner.finish_epoch_advance(next_epoch);
        Ok(EpisodeCommitOutcome::Applied(identity(node)))
    }

    async fn get_episode(&self, request: &EpisodeGet) -> Result<Option<EpisodeRecord>> {
        let inner = self.lock();
        let Some(&head) = inner.episode_projection.heads.get(&request.episode_id) else {
            return Ok(None);
        };
        let id = request.edition_id.unwrap_or(head);
        let Some(node) = inner.nodes.get(&id) else {
            return Ok(None);
        };
        if node
            .episode()
            .is_none_or(|facet| facet.root() != request.episode_id)
        {
            return Err(Error::InvalidInput(
                "requested edition does not belong to this episode".into(),
            ));
        }
        record(&inner, node).map(Some)
    }

    async fn episode_timeline(
        &self,
        request: &EpisodeTimelineRequest,
    ) -> Result<EpisodePage<EpisodeTimelineCursor>> {
        request
            .validate()
            .map_err(|e| Error::InvalidInput(e.to_string()))?;
        if let Some(cursor) = &request.after {
            cursor
                .validate(self.db_id, request)
                .map_err(|e| Error::InvalidInput(e.to_string()))?;
        }
        let inner = self.lock();
        let projection = &inner.episode_projection;
        let indexes = match request.axis {
            EpisodeTimelineAxis::Recorded => &projection.recorded,
            EpisodeTimelineAxis::Occurred => &projection.occurred,
        };
        let bucket = request
            .filter
            .thread
            .as_ref()
            .map(|thread| thread.as_str().to_owned());
        let Some(keys) = indexes.get(&bucket) else {
            return Ok(EpisodePage {
                items: vec![],
                next: None,
                partial: false,
            });
        };
        use std::ops::Bound::{Excluded, Included, Unbounded};
        let mut lower = Unbounded;
        let mut upper = Unbounded;
        // Occurrence windows are overlap tests. A long scene starting before
        // the lower bound can still overlap it, so only upper can trim starts.
        if let Some(window) = &request.window {
            if matches!(request.axis, EpisodeTimelineAxis::Recorded)
                && let Some(from) = window.from
            {
                lower = Included((from, EpisodeId::new(NodeId(Ulid::from(0u128)))));
            }
            if let Some(through) = window.through {
                upper = Included((through, EpisodeId::new(NodeId(Ulid::from(u128::MAX)))));
            }
        }
        if let Some(cursor) = &request.after {
            let key = cursor.key();
            match request.order {
                EpisodeOrder::OldestFirst => {
                    if matches!(lower, Unbounded)
                        || matches!(lower, Included(bound) | Excluded(bound) if bound <= key)
                    {
                        lower = Excluded(key);
                    }
                }
                EpisodeOrder::NewestFirst => {
                    if matches!(upper, Unbounded)
                        || matches!(upper, Included(bound) | Excluded(bound) if bound >= key)
                    {
                        upper = Excluded(key);
                    }
                }
            }
        }
        let empty = match (&lower, &upper) {
            (Included(a), Included(b)) => a > b,
            (Included(a) | Excluded(a), Included(b) | Excluded(b)) => a >= b,
            _ => false,
        };
        if empty {
            return Ok(EpisodePage {
                items: vec![],
                next: None,
                partial: false,
            });
        }
        let range = keys.range((lower, upper));
        let mut ordered: Box<dyn Iterator<Item = &(EpisodeTime, EpisodeId)> + '_> =
            match request.order {
                EpisodeOrder::OldestFirst => Box::new(range),
                EpisodeOrder::NewestFirst => Box::new(range.rev()),
            };
        let mut items = Vec::with_capacity(request.limit.get());
        let mut inspected = 0;
        let mut last = None;
        while inspected < TIMELINE_WORK_LIMIT {
            let Some(&(time, root)) = ordered.next() else {
                return Ok(EpisodePage {
                    items,
                    next: None,
                    partial: false,
                });
            };
            inspected += 1;
            let node = &inner.nodes[&projection.heads[&root]];
            let facet = node.episode().expect("indexed episode head");
            let admitted = request.matches(facet);
            if admitted {
                if items.len() == request.limit.get() {
                    // Lookahead is not consumed by the continuation.
                    return Ok(EpisodePage {
                        items,
                        next: last.map(|(at, root)| {
                            EpisodeTimelineCursor::new(self.db_id, request, at, root)
                        }),
                        partial: false,
                    });
                }
                items.push(header(node, node.id())?);
            }
            last = Some((time, root));
        }
        let more = ordered.next().is_some();
        Ok(EpisodePage {
            items,
            next: more.then(|| {
                let (time, root) = last.expect("positive work limit");
                EpisodeTimelineCursor::new(self.db_id, request, time, root)
            }),
            partial: more,
        })
    }

    async fn episode_cue(&self, request: &EpisodeCueRequest) -> Result<EpisodeCuePage> {
        request
            .validate()
            .map_err(|e| Error::InvalidInput(e.to_string()))?;
        let inner = self.lock();
        let projection = &inner.episode_projection;
        // This transparent reference lexical rank counts matching distinct
        // terms. Native FTS may rank differently; no shared score is exposed.
        let terms: BTreeSet<_> = lexical_terms(request.cue.as_str()).into_iter().collect();
        let mut scores = HashMap::<EpisodeId, usize>::new();
        for term in terms {
            if let Some(roots) = projection.lexical.get(&term) {
                for root in roots {
                    let node = &inner.nodes[&projection.heads[root]];
                    if request
                        .filter
                        .matches(node.episode().expect("indexed episode"))
                    {
                        *scores.entry(*root).or_default() += 1;
                    }
                }
            }
        }
        let mut ranked: Vec<_> = scores.into_iter().collect();
        ranked.sort_by(|(a, sa), (b, sb)| sb.cmp(sa).then_with(|| a.cmp(b)));
        let has_more = ranked.len() > request.limit.get();
        let items = ranked
            .into_iter()
            .take(request.limit.get())
            .map(|(root, _)| {
                let id = projection.heads[&root];
                header(&inner.nodes[&id], id)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(EpisodeCuePage {
            mode: EpisodeCueMode::Lexical,
            items,
            has_more,
            partial: false,
        })
    }

    async fn episode_history(
        &self,
        request: &EpisodeHistoryRequest,
    ) -> Result<EpisodePage<EpisodeHistoryCursor>> {
        if let Some(cursor) = &request.after {
            cursor
                .validate(self.db_id, request)
                .map_err(|e| Error::InvalidInput(e.to_string()))?;
        }
        let inner = self.lock();
        let projection = &inner.episode_projection;
        let Some(history) = projection.history.get(&request.episode_id) else {
            return Ok(EpisodePage {
                items: vec![],
                next: None,
                partial: false,
            });
        };
        use std::ops::Bound::{Excluded, Unbounded};
        let lower = request
            .after
            .as_ref()
            .map_or(Unbounded, |cursor| Excluded(cursor.key().0.get()));
        let mut records = history.range((lower, Unbounded));
        let mut items = Vec::with_capacity(request.limit.get());
        for (_, &id) in records.by_ref().take(request.limit.get()) {
            items.push(header(
                &inner.nodes[&id],
                projection.heads[&request.episode_id],
            )?);
        }
        let next = if records.next().is_some() {
            items.last().map(|last| {
                EpisodeHistoryCursor::new(
                    self.db_id,
                    request,
                    last.identity.revision,
                    last.identity.edition_id,
                )
            })
        } else {
            None
        };
        Ok(EpisodePage {
            items,
            next,
            partial: false,
        })
    }

    async fn episode_references(
        &self,
        request: &EpisodeReferencesRequest,
    ) -> Result<EpisodeReferencesPage> {
        if let Some(cursor) = &request.after {
            cursor
                .validate(self.db_id, request)
                .map_err(|e| Error::InvalidInput(e.to_string()))?;
        }
        let inner = self.lock();
        let mut edges = inner.incident_edges(request.anchor);
        edges.retain(|edge| {
            inner.is_episode_edge(edge)
                && request
                    .after
                    .as_ref()
                    .is_none_or(|cursor| (edge.from, edge.to) > cursor.key())
        });
        edges.sort_by_key(|edge| (edge.from, edge.to));
        let has_more = edges.len() > request.limit.get();
        let items: Vec<_> = edges
            .into_iter()
            .take(request.limit.get())
            .map(|edge| {
                let from_episode = inner
                    .nodes
                    .get(&edge.from)
                    .filter(|node| !node.is_semantic())
                    .map(identity);
                let to_episode = inner
                    .nodes
                    .get(&edge.to)
                    .filter(|node| !node.is_semantic())
                    .map(identity);
                EpisodeReference {
                    edge,
                    from_episode,
                    to_episode,
                }
            })
            .collect();
        let next = if has_more {
            items.last().map(|last| {
                EpisodeReferencesCursor::new(self.db_id, request, last.edge.from, last.edge.to)
            })
        } else {
            None
        };
        Ok(EpisodeReferencesPage { items, next })
    }
}

const TIMELINE_WORK_LIMIT: usize = 256;

fn header(node: &Node, current_edition_id: NodeId) -> Result<EpisodeHeader> {
    EpisodeHeader::from_node(node, current_edition_id).map_err(Into::into)
}

#[cfg(test)]
mod tests;

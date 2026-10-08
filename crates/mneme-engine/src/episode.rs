//! Source-authored episodes and immutable editorial editions.
//!
//! The episode lane shares canonical nodes and explicit links with semantic
//! memory, but not its promotion, inference or retention policy. A source key
//! denotes one immutable edition; editing uses a new key and an expected head.

use mneme_core::episode::{
    EpisodeCommit, EpisodeCommitOutcome, EpisodeCuePage, EpisodeCueRequest, EpisodeFacet,
    EpisodeGet, EpisodeHistoryCursor, EpisodeHistoryRequest, EpisodeId, EpisodeIdentity,
    EpisodePage, EpisodeRecord, EpisodeReferencesPage, EpisodeReferencesRequest, EpisodeRevision,
    EpisodeRevisionReason, EpisodeStore, EpisodeThread, EpisodeTime, EpisodeTimelineCursor,
    EpisodeTimelineRequest, EpisodeWriteExpectation, MAX_EPISODE_BODY_BYTES, MAX_EPISODE_LINKS,
    MAX_EPISODE_SUMMARY_BYTES, OccurrenceContexts, OccurrenceSpan,
};

use super::{
    BodyOwnership, Capture, CaptureLink, CaptureSource, Confidence, Edge, Error, Memory, Node,
    NodeId, NodeInit, NodeStatus, OriginCommit, PreparedCapture, Provenance, Result, Sha256,
    Stability,
};
use sha2::Digest;

/// A bounded authored scene. Capture-source fields identify this immutable
/// edition, not its episode's mutable current-head projection. Host recording
/// time and `origin_commit` are fixed on first application, never retry inputs.
pub struct EpisodeWrite<'a> {
    pub namespace: &'a str,
    pub key: &'a str,
    pub reference: &'a str,
    pub session: Option<&'a str>,
    pub revision: Option<&'a str>,
    pub summary: &'a str,
    pub body: &'a [u8],
    pub tags: &'a [&'a str],
    pub links: &'a [CaptureLink],
    pub occurrence: OccurrenceSpan,
    pub thread: Option<EpisodeThread>,
    pub occurrence_contexts: Option<OccurrenceContexts>,
    pub origin_commit: Option<OriginCommit>,
}

impl<'a> EpisodeWrite<'a> {
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        namespace: &'a str,
        key: &'a str,
        reference: &'a str,
        session: Option<&'a str>,
        revision: Option<&'a str>,
        summary: &'a str,
        body: &'a [u8],
        tags: &'a [&'a str],
        occurrence: OccurrenceSpan,
        thread: Option<EpisodeThread>,
    ) -> Self {
        Self {
            namespace,
            key,
            reference,
            session,
            revision,
            summary,
            body,
            tags,
            links: &[],
            occurrence,
            thread,
            occurrence_contexts: None,
            origin_commit: None,
        }
    }

    pub fn with_occurrence_contexts(mut self, contexts: OccurrenceContexts) -> Self {
        self.occurrence_contexts = Some(contexts);
        self
    }

    pub fn with_links(mut self, links: &'a [CaptureLink]) -> Self {
        self.links = links;
        self
    }

    /// Validate an initial write and compute its exact retry proof without
    /// entering a mutation gate, reading a clock, embedding or storing bodies.
    pub fn validated_source(self) -> Result<CaptureSource> {
        let mut request = PreparedEpisode::try_from(self)?;
        request.bind_intent(
            EpisodeId::new(request.capture.source.node_id()),
            None,
            EpisodeRevision::INITIAL,
            None,
        );
        Ok(request.capture.source)
    }

    /// Compute the proof for a known editorial ordinal. This is for readback
    /// verification; it is not an alternative write operation. The caller must
    /// obtain the ordinal from an immutable predecessor or returned edition.
    /// `Memory::revise_episode` always derives it from the stored predecessor.
    pub fn validated_revision_source(
        self,
        root: EpisodeId,
        expected_edition: NodeId,
        next_revision: EpisodeRevision,
        reason: &EpisodeRevisionReason,
    ) -> Result<CaptureSource> {
        let mut request = PreparedEpisode::try_from(self)?;
        if next_revision.get() == 0 {
            return Err(Error::InvalidInput(
                "an editorial revision must follow the initial edition".into(),
            ));
        }
        request.validate_edition_id(root, expected_edition)?;
        request.bind_intent(root, Some(expected_edition), next_revision, Some(reason));
        Ok(request.capture.source)
    }

    pub fn with_origin_commit(mut self, commit: Option<&str>) -> Result<Self> {
        self.origin_commit = commit
            .map(OriginCommit::parse)
            .transpose()
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EpisodeWriteResult {
    pub identity: EpisodeIdentity,
    pub replayed: bool,
}

struct PreparedEpisode<'a> {
    capture: PreparedCapture<'a>,
    occurrence: OccurrenceSpan,
    thread: Option<EpisodeThread>,
    occurrence_contexts: Option<OccurrenceContexts>,
}

impl<'a> TryFrom<EpisodeWrite<'a>> for PreparedEpisode<'a> {
    type Error = Error;

    fn try_from(request: EpisodeWrite<'a>) -> Result<Self> {
        if request.summary.len() > MAX_EPISODE_SUMMARY_BYTES {
            return Err(Error::InvalidInput(format!(
                "episode summary exceeds {MAX_EPISODE_SUMMARY_BYTES} bytes"
            )));
        }
        if request.body.len() > MAX_EPISODE_BODY_BYTES {
            return Err(Error::InvalidInput(format!(
                "episode body exceeds {MAX_EPISODE_BODY_BYTES} bytes"
            )));
        }
        if request.links.len() > MAX_EPISODE_LINKS {
            return Err(Error::InvalidInput(format!(
                "episode links exceed {MAX_EPISODE_LINKS} edges"
            )));
        }
        if request.tags.contains(&super::CORE_TAG) {
            return Err(Error::InvalidInput(
                "episodes cannot be always-loaded core memory".into(),
            ));
        }
        request
            .occurrence
            .validate()
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        let capture = PreparedCapture::with_codec(
            Capture {
                namespace: request.namespace,
                key: request.key,
                reference: request.reference,
                session: request.session,
                revision: request.revision,
                summary: request.summary,
                body: request.body,
                tags: request.tags,
                links: request.links,
                touchstone: None,
                stability: 0.5,
                confidence: 0.5,
                origin_commit: request.origin_commit,
            },
            if request.occurrence_contexts.is_some() {
                super::CaptureRequestCodec::EpisodeV2
            } else {
                super::CaptureRequestCodec::EpisodeV1
            },
        )?;
        Ok(Self {
            capture,
            occurrence: request.occurrence,
            thread: request.thread,
            occurrence_contexts: request.occurrence_contexts,
        })
    }
}

impl PreparedEpisode<'_> {
    fn validate_edition_id(&self, root: EpisodeId, expected_edition: NodeId) -> Result<()> {
        let id = self.capture.source.node_id();
        if id == expected_edition || id == root.node_id() {
            return Err(Error::Conflict(
                "episode revision requires a new source key".into(),
            ));
        }
        Ok(())
    }

    /// Reuse capture's canonical author-content proof as a nested component;
    /// the outer domain prevents semantic capture/episode replay equivalence.
    /// No clock or host checkout value participates in this identity.
    fn bind_intent(
        &mut self,
        root: EpisodeId,
        predecessor: Option<NodeId>,
        revision: EpisodeRevision,
        reason: Option<&EpisodeRevisionReason>,
    ) {
        let mut hash = Sha256::new();
        let codec = if self.occurrence_contexts.is_some() {
            hash.update(b"mneme.episode.request.v2\0");
            super::CaptureRequestCodec::EpisodeV2
        } else {
            hash.update(b"mneme.episode.request.v1\0");
            super::CaptureRequestCodec::EpisodeV1
        };
        hash.update(self.capture.source.request_digest());
        hash.update(root.node_id().0.to_bytes());
        match predecessor {
            Some(id) => {
                hash.update([1]);
                hash.update(id.0.to_bytes());
            }
            None => hash.update([0]),
        }
        hash.update(revision.get().to_be_bytes());
        match self.occurrence {
            OccurrenceSpan::Unknown => hash.update([0]),
            OccurrenceSpan::Point { at } => {
                hash.update([1]);
                hash.update(at.get().to_be_bytes());
            }
            OccurrenceSpan::Range { start, end } => {
                hash.update([2]);
                hash.update(start.get().to_be_bytes());
                hash.update(end.get().to_be_bytes());
            }
        }
        match &self.thread {
            Some(thread) => {
                hash.update([1]);
                hash.update((thread.as_str().len() as u64).to_be_bytes());
                hash.update(thread.as_str().as_bytes());
            }
            None => hash.update([0]),
        }
        match reason {
            Some(reason) => {
                hash.update([1]);
                hash.update((reason.as_str().len() as u64).to_be_bytes());
                hash.update(reason.as_str().as_bytes());
            }
            None => hash.update([0]),
        }
        if let Some(contexts) = &self.occurrence_contexts {
            fn part(hash: &mut Sha256, value: &str) {
                hash.update((value.len() as u64).to_be_bytes());
                hash.update(value.as_bytes());
            }
            hash.update((contexts.as_slice().len() as u64).to_be_bytes());
            for context in contexts.iter() {
                part(&mut hash, context.namespace());
                part(&mut hash, context.key());
                match context.label() {
                    Some(label) => {
                        hash.update([1]);
                        part(&mut hash, label);
                    }
                    None => hash.update([0]),
                }
            }
        }
        let source = &self.capture.source;
        self.capture.source = CaptureSource::new_with_codec(
            source.namespace(),
            source.key(),
            source.reference(),
            source.session(),
            source.revision(),
            hash.finalize().into(),
            codec,
        )
        .expect("capture preparation validated source fields");
    }
}

impl Memory {
    /// Append one episode root. An exact source-key replay returns the original
    /// edition, even if the episode has since acquired an editorial successor.
    pub async fn append_episode(&self, request: EpisodeWrite<'_>) -> Result<EpisodeWriteResult> {
        let mut request = PreparedEpisode::try_from(request)?;
        let episodes = self.episode_store()?;
        let root = EpisodeId::new(request.capture.source.node_id());
        request.bind_intent(root, None, EpisodeRevision::INITIAL, None);
        let _mutation = self.mutation_gate.lock().await;
        if let Some(record) = episodes.lookup_episode(&request.capture.source).await? {
            return self.episode_replay(record, request.capture.body).await;
        }
        let now = EpisodeTime::new(self.clock.now())
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        let mut facet = EpisodeFacet::initial(
            root.node_id(),
            request.occurrence.clone(),
            request.thread.clone(),
            now,
        )?;
        if let Some(contexts) = &request.occurrence_contexts {
            facet = facet.with_occurrence_contexts(contexts.clone());
        }
        self.publish_episode(
            episodes,
            request,
            facet,
            now,
            EpisodeWriteExpectation::NewRoot,
        )
        .await
    }

    /// Append an editorial edition using an explicit immutable predecessor.
    /// A stale expected head conflicts; this operation never rebases an edit.
    pub async fn revise_episode(
        &self,
        root: EpisodeId,
        expected_edition: NodeId,
        reason: EpisodeRevisionReason,
        request: EpisodeWrite<'_>,
    ) -> Result<EpisodeWriteResult> {
        let mut request = PreparedEpisode::try_from(request)?;
        let episodes = self.episode_store()?;
        // Read the named immutable edition, not a freshly resolved head. This
        // fixes the next ordinal and authored request digest before mutation.
        let predecessor = episodes
            .get_episode(&EpisodeGet {
                episode_id: root,
                edition_id: Some(expected_edition),
            })
            .await?
            .ok_or(Error::NotFound)?;
        let previous = predecessor.node.episode().ok_or_else(|| {
            Error::Conflict("episode predecessor has no canonical episode facet".into())
        })?;
        let mut facet = EpisodeFacet::revised(
            root,
            expected_edition,
            previous.revision().next()?,
            request.occurrence.clone(),
            request.thread.clone(),
            previous.recorded_at(),
            reason.clone(),
        )?;
        if let Some(contexts) = &request.occurrence_contexts {
            facet = facet.with_occurrence_contexts(contexts.clone());
        }
        request.validate_edition_id(root, expected_edition)?;
        request.bind_intent(
            root,
            Some(expected_edition),
            facet.revision(),
            Some(&reason),
        );
        let _mutation = self.mutation_gate.lock().await;
        if let Some(record) = episodes.lookup_episode(&request.capture.source).await? {
            return self.episode_replay(record, request.capture.body).await;
        }
        let now = EpisodeTime::new(self.clock.now())
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        self.publish_episode(
            episodes,
            request,
            facet,
            now,
            EpisodeWriteExpectation::CurrentEdition(expected_edition),
        )
        .await
    }

    fn episode_store(&self) -> Result<&dyn EpisodeStore> {
        self.graph.episodes().ok_or_else(|| {
            Error::EpisodeUnavailable(
                mneme_core::episode::EpisodeUnavailableReason::AdapterUnsupported,
            )
        })
    }

    async fn episode_replay(
        &self,
        record: EpisodeRecord,
        expected_body: &[u8],
    ) -> Result<EpisodeWriteResult> {
        let body = self
            .bodies
            .resolve_range(record.node.body(), 0, expected_body.len())
            .await
            .map_err(|error| {
                Error::Conflict(format!("episode body unavailable on replay: {error}"))
            })?;
        if body.bytes != expected_body || body.next_offset.is_some() {
            return Err(Error::Conflict(format!(
                "episode body changed since the committed request for {}",
                record.identity.edition_id.0
            )));
        }
        Ok(EpisodeWriteResult {
            identity: record.identity,
            replayed: true,
        })
    }

    async fn publish_episode(
        &self,
        episodes: &dyn EpisodeStore,
        request: PreparedEpisode<'_>,
        facet: EpisodeFacet,
        now: EpisodeTime,
        expectation: EpisodeWriteExpectation,
    ) -> Result<EpisodeWriteResult> {
        let embedding = self.embed_one(request.capture.summary.as_str()).await?;
        let body_ref = self
            .bodies
            .put(self.cfg.default_body_scheme, request.capture.body)
            .await?;
        let id = request.capture.source.node_id();
        let source = request.capture.source.clone();
        let node = Node::new(NodeInit {
            id,
            summary: request.capture.summary,
            body: body_ref.clone(),
            tags: request.capture.tags,
            provenance: Provenance::External {
                source: request.capture.source,
            },
            stability: Stability::new(0.5).expect("constant stability"),
            confidence: Confidence::new(0.5).expect("constant confidence"),
            status: NodeStatus::Active,
            created: now.get(),
        })
        .with_body_ownership(BodyOwnership::Managed)
        .with_origin_commit(request.capture.origin_commit)
        .with_episode(facet)
        .map_err(|error| Error::InvalidInput(error.to_string()))?;
        let links: Vec<_> = request
            .capture
            .links
            .iter()
            .map(|link| Edge::new(id, link.to, link.weight, link.kind, node.created()))
            .collect();
        match episodes
            .commit_episode(EpisodeCommit {
                node: &node,
                embedding: &embedding,
                links: &links,
                expectation,
            })
            .await
        {
            Ok(EpisodeCommitOutcome::Applied(identity)) => Ok(EpisodeWriteResult {
                identity,
                replayed: false,
            }),
            Ok(EpisodeCommitOutcome::AlreadyApplied(_)) => {
                // A different Memory instance won after our probe. Verify its
                // actual body rather than treating a backend marker as proof.
                // BodyStore::put need not return a globally fresh reference:
                // without exclusive ownership, deleting our ref could erase
                // another canonical node's content. Retain a possible orphan.
                let original = episodes.lookup_episode(&source).await?.ok_or_else(|| {
                    Error::Conflict("episode replay proof vanished after commit".into())
                })?;
                self.episode_replay(original, request.capture.body).await
            }
            Err(error) => {
                // Backend failure may occur after publication. Even a definite
                // conflict does not prove exclusive body ownership. Retain the
                // possible orphan rather than erase shared/live content.
                Err(error)
            }
        }
    }

    pub async fn get_episode(&self, request: &EpisodeGet) -> Result<Option<EpisodeRecord>> {
        self.episode_store()?.get_episode(request).await
    }

    pub async fn episode_timeline(
        &self,
        request: &EpisodeTimelineRequest,
    ) -> Result<EpisodePage<EpisodeTimelineCursor>> {
        self.episode_store()?.episode_timeline(request).await
    }

    pub async fn episode_cue(&self, request: &EpisodeCueRequest) -> Result<EpisodeCuePage> {
        self.episode_store()?.episode_cue(request).await
    }

    pub async fn episode_history(
        &self,
        request: &EpisodeHistoryRequest,
    ) -> Result<EpisodePage<EpisodeHistoryCursor>> {
        self.episode_store()?.episode_history(request).await
    }

    /// Raw bounded one-hop navigation read; None means optional graph read
    /// capability unavailable, not an empty or filtered incident set.
    pub async fn incident_edges_page(
        &self,
        request: &mneme_core::ports::IncidentEdgesRequest,
    ) -> Result<Option<mneme_core::ports::IncidentEdgesPage>> {
        request.validate()?;
        self.graph.incident_edges_page(request).await
    }

    /// Read the exact cited immutable edition and its observed head, without
    /// loading bodies or minting learning/feedback authority.
    pub async fn episode_header_by_edition(
        &self,
        request: &mneme_core::episode::EpisodeHeaderByEditionRequest,
    ) -> Result<mneme_core::episode::EpisodeHeaderByEdition> {
        request.validate()?;
        self.episode_store()?
            .episode_header_by_edition(request)
            .await
    }

    pub async fn episode_references(
        &self,
        request: &EpisodeReferencesRequest,
    ) -> Result<EpisodeReferencesPage> {
        self.episode_store()?.episode_references(request).await
    }
}

//! Ordinary recall combines semantic knowledge and an explicitly episodic lane.
//! Retrieval policies stay separate; one presenter owns their shared context budget.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::Future,
    num::NonZeroU16,
    time::{Duration, Instant},
};

use mneme_core::episode::{
    EpisodeCue, EpisodeCuePage, EpisodeCueRequest, EpisodeFilter, EpisodeHeader,
    EpisodeHeaderByEdition, EpisodeHeaderByEditionRequest, EpisodePageLimit,
    EpisodeUnavailableReason, MAX_EPISODE_CUE_BYTES, MAX_EPISODE_PAGE_ITEMS,
};
use mneme_core::ports::{
    Budget, Error, IncidentEdgesCursor, IncidentEdgesPage, IncidentEdgesRequest, RoutingBinding,
    RoutingDiagnostics, RoutingHint, RoutingOutcome, StatusFilter, TraversalHop,
};
use mneme_core::{
    NodeId, SummarySnapshot, TouchstoneCursor, TouchstonePage, TouchstonePageRequest,
    TouchstoneRecord, TouchstoneReferrersRequest,
};
use mneme_engine::Memory;
use mneme_present::{
    CoreInputCard, EpisodeAnchor, EpisodeInputCard, EpisodeReferenceCounts,
    EpisodeReferenceRetrieval, EpisodeReferenceStopReason, EpisodicRetrieval,
    EpisodicUnavailableReason, ExpansionInputCard, Lane, LaneWindow, PackPlan, PackingInput,
    PresentationBudget, PrimaryInputCard, TouchstoneOrigin, TouchstoneReferenceView,
    TouchstoneResolution, TouchstoneRetrieval, TouchstoneStopReason, TouchstoneView, pack,
};

use crate::{ContextError, presentation_retrieval_metadata};

const MAX_LINKED_CAPACITY: usize = 256;
const LINKED_READ_TIMEOUT: Duration = Duration::from_secs(5);

fn lexical_capacity(requested: usize) -> usize {
    requested.min(MAX_EPISODE_PAGE_ITEMS)
}

/// Query-local mechanical provenance for one exact emitted card. A missing
/// path means no attributable graph contribution: direct, conditional-entry and
/// episodic cards do not invent learning routes. Their typed reference origins
/// are navigation evidence, not feedback receipts. A conditional binding identifies
/// a historical recommendation, not a currently walked path or feedback receipt.
#[derive(Clone, Debug)]
pub struct ContextObservation {
    pub node_id: NodeId,
    pub lane: Lane,
    pub card_sha256: String,
    pub graph_path: Option<Vec<TraversalHop>>,
    pub routing_binding: Option<RoutingBinding>,
    pub conditional_binding: Option<RoutingBinding>,
}

/// One packed bounded context plus opt-in shadow observations. The caller
/// still owns delivery: filtering these cards again requires intersecting the
/// observations with the actual delivered IDs and card digests.
#[derive(Clone, Debug)]
pub struct ObservedContext {
    pub plan: PackPlan,
    pub observations: Vec<ContextObservation>,
    pub routing: Option<RoutingDiagnostics>,
}

/// Retrieve semantic knowledge and current episodic scenes, then pack them once.
///
/// Current lexical scenes and exact referenced editions remain distinct from
/// semantic scoring. One hop from the frozen semantic/lexical anchors can add
/// historical scenes; those discoveries never become new anchors. It does not
/// create receipts, record exposure, fetch bodies, or train either memory lane.
///
/// Tags currently select the semantic lane only. Rather than ignoring that
/// selector or post-filtering an incomplete top-k, tagged requests explicitly
/// report that lexical episode search was skipped. The selected semantic
/// anchors can still yield indirect scenes without claiming episode tag matches.
pub async fn recall_context(
    memory: &Memory,
    text: &str,
    k: usize,
    retrieval_budget: Budget,
    tags: &[&str],
    presentation_budget: &PresentationBudget,
) -> Result<PackPlan, ContextError> {
    Ok(recall_context_inner(
        memory,
        text,
        k,
        retrieval_budget,
        tags,
        presentation_budget,
        false,
        None,
    )
    .await?
    .plan)
}

/// Opt into actual winning graph provenance without changing ordinary recall's
/// ranking or memory state. Only final packed cards are observed; a frontend
/// may repack this same window to fit mandatory observation bytes.
pub async fn recall_context_observed(
    memory: &Memory,
    text: &str,
    k: usize,
    retrieval_budget: Budget,
    tags: &[&str],
    presentation_budget: &PresentationBudget,
) -> Result<ObservedContext, ContextError> {
    recall_context_inner(
        memory,
        text,
        k,
        retrieval_budget,
        tags,
        presentation_budget,
        true,
        None,
    )
    .await
}

/// Explicit read-time routing opt-in; ordinary recall output remains unchanged.
#[expect(clippy::too_many_arguments)]
pub async fn recall_context_routed(
    memory: &Memory,
    text: &str,
    k: usize,
    retrieval_budget: Budget,
    tags: &[&str],
    presentation_budget: &PresentationBudget,
    hints: &[RoutingHint],
) -> Result<ObservedContext, ContextError> {
    recall_context_inner(
        memory,
        text,
        k,
        retrieval_budget,
        tags,
        presentation_budget,
        true,
        Some(hints),
    )
    .await
}

/// One immutable, bounded retrieval window. Repacking never retrieves again,
/// changes original-window omission counts, or consumes provenance metadata.
#[derive(Clone, Debug)]
pub struct ContextWindow {
    input: PackingInput,
    graph_paths: BTreeMap<NodeId, Vec<TraversalHop>>,
    routing: Option<RoutingOutcome>,
    observed: bool,
    navigation_only: BTreeSet<NodeId>,
}

impl ContextWindow {
    pub fn pack(&self, budget: &PresentationBudget) -> Result<ObservedContext, ContextError> {
        let plan = pack(budget, &self.input)?;
        let observations = if self.observed {
            plan.manifest()
                .cards()
                .iter()
                .map(|card| {
                    let navigation = self.navigation_only.contains(&card.node_id());
                    let conditional_binding = if card.lane() == Lane::Primary && !navigation {
                        self.routing
                            .as_ref()
                            .and_then(|routing| routing.conditional_bindings.get(&card.node_id()))
                            .cloned()
                    } else {
                        None
                    };
                    let conditional = conditional_binding.is_some();
                    ContextObservation {
                        node_id: card.node_id(),
                        lane: card.lane(),
                        card_sha256: card.card_sha256().to_owned(),
                        routing_binding: if conditional || navigation {
                            None
                        } else {
                            self.routing
                                .as_ref()
                                .and_then(|routing| routing.bindings.get(&card.node_id()))
                                .cloned()
                        },
                        graph_path: if card.lane() == Lane::Primary && !conditional && !navigation {
                            self.graph_paths.get(&card.node_id()).cloned()
                        } else {
                            None
                        },
                        conditional_binding,
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(ObservedContext {
            plan,
            observations,
            routing: self
                .routing
                .as_ref()
                .map(|routing| routing.diagnostics.clone()),
        })
    }
}

async fn recall_context_inner(
    memory: &Memory,
    text: &str,
    k: usize,
    retrieval_budget: Budget,
    tags: &[&str],
    presentation_budget: &PresentationBudget,
    observed: bool,
    routing_hints: Option<&[RoutingHint]>,
) -> Result<ObservedContext, ContextError> {
    prepare_context_window(
        memory,
        text,
        k,
        retrieval_budget,
        tags,
        observed,
        routing_hints,
    )
    .await?
    .pack(presentation_budget)
}

/// Retrieve exactly once; frontends may purely repack this window to include
/// their exact observation/control encoding inside the final byte envelope.
pub async fn prepare_context_window(
    memory: &Memory,
    text: &str,
    k: usize,
    retrieval_budget: Budget,
    tags: &[&str],
    observed: bool,
    routing_hints: Option<&[RoutingHint]>,
) -> Result<ContextWindow, ContextError> {
    let batch = if let Some(hints) = routing_hints {
        memory
            .retrieve_batch_seeded_routed(
                text,
                k,
                retrieval_budget,
                StatusFilter::default(),
                tags,
                hints,
            )
            .await?
    } else if observed {
        memory
            .retrieve_batch_seeded_observed(
                text,
                k,
                retrieval_budget,
                StatusFilter::default(),
                tags,
            )
            .await?
    } else {
        memory
            .retrieve_batch_seeded(text, k, retrieval_budget, StatusFilter::default(), tags)
            .await?
    };
    let retrieval = presentation_retrieval_metadata(&batch)?;
    let capacity = retrieval_budget.max_nodes.min(MAX_LINKED_CAPACITY);
    let (episodic_retrieval, lexical) = recall_episodes(memory, text, tags, capacity).await?;
    let semantic_anchors = batch
        .primary
        .iter()
        .map(|hit| hit.node.id())
        .collect::<Vec<_>>();
    let linked_deadline = Instant::now() + LINKED_READ_TIMEOUT;
    let frozen_lexical = lexical
        .cards()
        .iter()
        .map(EpisodeInputCard::id)
        .collect::<Vec<_>>();
    let (reference_retrieval, episodes) = compose_linked_scenes(
        memory,
        &semantic_anchors,
        lexical,
        capacity,
        LINKED_READ_TIMEOUT,
    )
    .await?;

    let frozen = interleave(semantic_anchors.clone(), frozen_lexical);
    let touchstone_allowance =
        (2 * capacity).saturating_sub(reference_retrieval.counts().endpoint_reads as usize);
    let touchstones = compose_touchstones(
        memory,
        &frozen,
        &semantic_anchors,
        touchstone_allowance,
        linked_deadline,
    )
    .await?;
    let navigation_only = touchstones
        .incoming
        .iter()
        .map(PrimaryInputCard::id)
        .filter(|id| !semantic_anchors.contains(id))
        .collect::<BTreeSet<_>>();
    let routing = batch.routing;
    let mut graph_paths = BTreeMap::new();

    let primary = batch
        .primary
        .into_iter()
        .map(|hit| {
            if let Some(path) = hit.graph_path {
                graph_paths.insert(hit.node.id(), path);
            }
            let mut card = PrimaryInputCard::new(hit.node.id(), hit.lane_rank, hit.node.summary())?;
            if let Some(view) = touchstones.views.get(&hit.node.id()) {
                card = card.with_touchstone(view.clone());
            }
            Ok(card)
        })
        .collect::<Result<Vec<_>, mneme_present::InputError>>()?;
    // Keep one ordinary task hit first. Incoming authored annotations are only
    // navigation candidates, never popularity/importance boosts or tag matches.
    let mut original = primary.into_iter();
    let mut primary = original.next().into_iter().collect::<Vec<_>>();
    primary.extend(interleave(original.collect(), touchstones.incoming));
    let primary = primary
        .into_iter()
        .enumerate()
        .map(|(index, card)| {
            card.with_rank(NonZeroU16::new((index + 1) as u16).expect("bounded semantic window"))
        })
        .collect();
    let input = PackingInput::new(
        retrieval,
        LaneWindow::<CoreInputCard>::complete(Vec::new()),
        LaneWindow::bounded(primary, true),
        LaneWindow::<ExpansionInputCard>::complete(Vec::new()),
    )?
    .with_episodic(episodic_retrieval, episodes)?
    .with_episode_reference_retrieval(reference_retrieval)?
    .with_touchstone_retrieval(touchstones.coverage)?;
    Ok(ContextWindow {
        input,
        graph_paths,
        routing,
        observed,
        navigation_only,
    })
}

async fn recall_episodes(
    memory: &Memory,
    text: &str,
    tags: &[&str],
    capacity: usize,
) -> Result<(EpisodicRetrieval, LaneWindow<EpisodeInputCard>), ContextError> {
    if !tags.is_empty() {
        return Ok((
            EpisodicRetrieval::not_searched_tag_filter(),
            LaneWindow::complete(Vec::new()),
        ));
    }
    if capacity == 0 {
        return Ok((
            EpisodicRetrieval::not_searched(),
            LaneWindow::complete(Vec::new()),
        ));
    }
    let Some((cue, normalized, truncated)) = normalize_episode_cue(text) else {
        // A control-only query has no textual cue. Do not invent a search term
        // or turn a valid semantic request into a new episode validation error.
        return Ok((
            EpisodicRetrieval::not_searched(),
            LaneWindow::complete(Vec::new()),
        ));
    };
    let request = EpisodeCueRequest {
        cue,
        filter: EpisodeFilter::default(),
        limit: EpisodePageLimit::new(lexical_capacity(capacity))
            .expect("capacity is clamped to the existing cue-page fuse"),
    };
    episode_result(
        memory.episode_cue(&request).await,
        normalized,
        truncated,
        lexical_capacity(capacity),
    )
}

fn episode_result(
    result: Result<EpisodeCuePage, Error>,
    normalized: bool,
    truncated: bool,
    capacity: usize,
) -> Result<(EpisodicRetrieval, LaneWindow<EpisodeInputCard>), ContextError> {
    let page = match result {
        Ok(page) => page,
        Err(Error::EpisodeUnavailable(reason)) => {
            let reason = match reason {
                EpisodeUnavailableReason::AdapterUnsupported => {
                    EpisodicUnavailableReason::AdapterUnsupported
                }
                EpisodeUnavailableReason::StoreNotUpgraded => {
                    EpisodicUnavailableReason::StoreNotUpgraded
                }
            };
            return Ok((
                EpisodicRetrieval::unavailable(reason),
                LaneWindow::complete(Vec::new()),
            ));
        }
        Err(error) => return Err(error.into()),
    };
    if page.items.len() > capacity {
        return Err(
            Error::Backend("episode cue exceeded its requested bounded window".into()).into(),
        );
    }
    let further_tail_unknown = page.has_more || page.partial;
    let cards = page
        .items
        .into_iter()
        .enumerate()
        .map(|(index, header)| {
            let rank = NonZeroU16::new((index + 1) as u16)
                .expect("bounded episode result ranks are positive");
            EpisodeInputCard::new(header, rank)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((
        EpisodicRetrieval::searched(normalized, truncated),
        LaneWindow::bounded(cards, further_tail_unknown),
    ))
}

/// Narrow query-local read seam: no mutation, semantic neighbours, body fetch,
/// feedback receipt or recursive retrieval is available to this composer.
trait LinkedSceneReader: Sync {
    fn incident_page(
        &self,
        request: &IncidentEdgesRequest,
    ) -> impl Future<Output = Result<Option<IncidentEdgesPage>, Error>> + Send;
    fn exact_header(
        &self,
        request: &EpisodeHeaderByEditionRequest,
    ) -> impl Future<Output = Result<EpisodeHeaderByEdition, Error>> + Send;
}

impl LinkedSceneReader for Memory {
    async fn incident_page(
        &self,
        request: &IncidentEdgesRequest,
    ) -> Result<Option<IncidentEdgesPage>, Error> {
        self.incident_edges_page(request).await
    }
    async fn exact_header(
        &self,
        request: &EpisodeHeaderByEditionRequest,
    ) -> Result<EpisodeHeaderByEdition, Error> {
        self.episode_header_by_edition(request).await
    }
}

#[derive(Clone)]
enum CachedEndpoint {
    Missing,
    Semantic,
    Failed,
    Episode(EpisodeHeader),
}

struct FrozenAnchor {
    origin: EpisodeAnchor,
    id: NodeId,
    cursor: Option<IncidentEdgesCursor>,
}

/// Keep two initial discovery lanes fair without assigning relevance to scene
/// counts. This is also the presentation order after same-edition origin union.
fn interleave<T>(left: Vec<T>, right: Vec<T>) -> Vec<T> {
    let mut left = left.into_iter();
    let mut right = right.into_iter();
    let mut result = Vec::new();
    loop {
        let a = left.next();
        let b = right.next();
        if a.is_none() && b.is_none() {
            break;
        }
        result.extend(a);
        result.extend(b);
    }
    result
}

fn remaining(deadline: Instant) -> Option<Duration> {
    let duration = deadline.saturating_duration_since(Instant::now());
    (!duration.is_zero()).then_some(duration)
}

async fn compose_linked_scenes(
    reader: &impl LinkedSceneReader,
    semantic: &[NodeId],
    lexical: LaneWindow<EpisodeInputCard>,
    capacity: usize,
    timeout: Duration,
) -> Result<(EpisodeReferenceRetrieval, LaneWindow<EpisodeInputCard>), ContextError> {
    let capacity = capacity.min(MAX_LINKED_CAPACITY);
    let deadline = Instant::now() + timeout.min(LINKED_READ_TIMEOUT);
    let mut cache = semantic
        .iter()
        .map(|id| (*id, CachedEndpoint::Semantic))
        .collect::<BTreeMap<_, _>>();
    let lexical_cards = lexical.cards().to_vec();
    for card in &lexical_cards {
        let header = card.header();
        cache.insert(header.identity.edition_id, CachedEndpoint::Episode(header));
    }
    let semantic_anchors = semantic
        .iter()
        .map(|id| FrozenAnchor {
            origin: EpisodeAnchor::Semantic { node_id: *id },
            id: *id,
            cursor: None,
        })
        .collect();
    let lexical_anchors = lexical_cards
        .iter()
        .map(|card| {
            let identity = card.identity();
            FrozenAnchor {
                origin: EpisodeAnchor::Episode { identity },
                id: identity.edition_id,
                cursor: None,
            }
        })
        .collect();
    let mut anchors = VecDeque::from(interleave(semantic_anchors, lexical_anchors));
    let mut counts = EpisodeReferenceCounts {
        anchors_total: anchors.len() as u32,
        raw_edge_limit: (2 * capacity) as u32,
        endpoint_read_limit: capacity as u32,
        ..EpisodeReferenceCounts::default()
    };
    if anchors.is_empty() {
        return Ok((EpisodeReferenceRetrieval::not_searched(), lexical));
    }
    let mut examined = BTreeSet::new();
    let mut stop = None;
    let mut references: Vec<EpisodeInputCard> = Vec::new();
    let mut reference_indices: BTreeMap<_, usize> = BTreeMap::new();
    // Single-row round-robin pages charge raw work even for non-episodes,
    // missing endpoints and duplicates. Both physical leg cursors live in each
    // anchor, so fairness never rescans a hub from the beginning. Each admitted
    // raw row needs at most two indexed leg seeks; each anchor has at most two
    // final empty-leg seeks. Edge/body-anchor point reads come from the adapter,
    // and each uncached endpoint operation has fixed node/head overhead.
    while let Some(mut anchor) = anchors.pop_front() {
        if counts.raw_edges_scanned >= counts.raw_edge_limit {
            stop.get_or_insert(EpisodeReferenceStopReason::Budget);
            anchors.push_front(anchor);
            break;
        }
        let Some(duration) = remaining(deadline) else {
            stop = Some(EpisodeReferenceStopReason::Deadline);
            anchors.push_front(anchor);
            break;
        };
        examined.insert(anchor.id);
        let request = IncidentEdgesRequest::new(anchor.id, 1, duration, anchor.cursor.clone())?;
        let page = match reader.incident_page(&request).await {
            Ok(Some(page)) => page,
            Ok(None) => {
                stop = Some(EpisodeReferenceStopReason::Unsupported);
                anchors.push_front(anchor);
                break;
            }
            Err(_) => {
                stop = Some(if remaining(deadline).is_none() {
                    EpisodeReferenceStopReason::Deadline
                } else {
                    EpisodeReferenceStopReason::ReadError
                });
                // Err may follow admitted raw work without returning its counts.
                // Stop the entire linked stage at the first failed page: this
                // final one-row request had >=1 row of remaining allowance, so
                // even hidden failed work stays inside 2N. Work counters cover
                // completed operations only on read_error, never invented work.
                counts.further_tail_unknown = true;
                anchors.push_front(anchor);
                break;
            }
        };
        counts.raw_edges_scanned += page.work.rows_scanned as u32;
        counts.indexed_seeks += page.work.indexed_seeks as u32;
        counts.edge_point_reads += page.work.edge_point_reads as u32;
        counts.body_anchor_point_reads += page.work.body_anchor_point_reads as u32;
        if remaining(deadline).is_none() {
            stop = Some(EpisodeReferenceStopReason::Deadline);
            counts.further_tail_unknown = true;
            break;
        }
        if page.work.rows_scanned > 1
            || page.items.len() > page.work.rows_scanned
            || page.next.as_ref().is_some_and(|next| {
                anchor
                    .cursor
                    .as_ref()
                    .unwrap_or(&IncidentEdgesCursor::new(anchor.id))
                    == next
            })
        {
            stop.get_or_insert(EpisodeReferenceStopReason::ReadError);
            counts.further_tail_unknown = true;
            continue;
        }
        for edge in page.items {
            let endpoint = if edge.from == anchor.id {
                edge.to
            } else if edge.to == anchor.id {
                edge.from
            } else {
                stop.get_or_insert(EpisodeReferenceStopReason::ReadError);
                counts.further_tail_unknown = true;
                continue;
            };
            let outcome = if let Some(cached) = cache.get(&endpoint) {
                counts.cache_hits += 1;
                cached.clone()
            } else {
                if counts.endpoint_reads >= counts.endpoint_read_limit {
                    stop.get_or_insert(EpisodeReferenceStopReason::Budget);
                    counts.further_tail_unknown = true;
                    continue;
                }
                let Some(duration) = remaining(deadline) else {
                    stop = Some(EpisodeReferenceStopReason::Deadline);
                    counts.further_tail_unknown = true;
                    break;
                };
                counts.endpoint_reads += 1;
                let request = EpisodeHeaderByEditionRequest::new(endpoint, duration)?;
                let outcome = match reader.exact_header(&request).await {
                    Ok(EpisodeHeaderByEdition::Missing) => {
                        counts.missing += 1;
                        CachedEndpoint::Missing
                    }
                    Ok(EpisodeHeaderByEdition::Semantic) => {
                        counts.non_episode += 1;
                        CachedEndpoint::Semantic
                    }
                    Ok(EpisodeHeaderByEdition::Episode(header)) => CachedEndpoint::Episode(header),
                    Err(Error::EpisodeUnavailable(_)) => {
                        stop = Some(EpisodeReferenceStopReason::Unsupported);
                        counts.further_tail_unknown = true;
                        break;
                    }
                    Err(_) => {
                        stop.get_or_insert(if remaining(deadline).is_none() {
                            EpisodeReferenceStopReason::Deadline
                        } else {
                            EpisodeReferenceStopReason::ReadError
                        });
                        counts.further_tail_unknown = true;
                        cache.insert(endpoint, CachedEndpoint::Failed);
                        continue;
                    }
                };
                cache.insert(endpoint, outcome.clone());
                if remaining(deadline).is_none() {
                    stop = Some(EpisodeReferenceStopReason::Deadline);
                    counts.further_tail_unknown = true;
                    break;
                }
                outcome
            };
            if let CachedEndpoint::Episode(header) = outcome {
                let rank = NonZeroU16::new(1).unwrap();
                let card = match EpisodeInputCard::referenced(
                    header,
                    rank,
                    anchor.origin.clone(),
                    &edge,
                ) {
                    Ok(card) => card,
                    Err(_) => {
                        stop.get_or_insert(EpisodeReferenceStopReason::ReadError);
                        counts.further_tail_unknown = true;
                        continue;
                    }
                };
                let identity = card.identity();
                let key = (identity.episode_id, identity.edition_id);
                if let Some(&index) = reference_indices.get(&key) {
                    references[index].merge_origins(&card)?;
                } else {
                    reference_indices.insert(key, references.len());
                    references.push(card);
                }
            }
        }
        if let Some(cursor) = page.next {
            anchor.cursor = Some(cursor);
            anchors.push_back(anchor);
        }
        if matches!(
            stop,
            Some(EpisodeReferenceStopReason::Deadline | EpisodeReferenceStopReason::Unsupported)
        ) {
            break;
        }
    }
    counts.anchors_examined = examined.len() as u32;
    counts.unread_anchors = counts.anchors_total - counts.anchors_examined;
    counts.further_tail_unknown |= !anchors.is_empty() || stop.is_some();
    let coverage = EpisodeReferenceRetrieval::searched(counts, stop)?;
    // Union shared lexical/reference identities before interleaving the two
    // discovery lanes: duplicate references must not consume fair slots and
    // strand a distinct indirect scene behind a long lexical prefix.
    let mut lexical_cards = lexical_cards;
    let lexical_indices = lexical_cards
        .iter()
        .enumerate()
        .map(|(index, card)| {
            let identity = card.identity();
            ((identity.episode_id, identity.edition_id), index)
        })
        .collect::<BTreeMap<_, _>>();
    let mut indirect = Vec::new();
    for card in references {
        let identity = card.identity();
        let key = (identity.episode_id, identity.edition_id);
        if let Some(&index) = lexical_indices.get(&key) {
            lexical_cards[index].merge_origins(&card)?;
        } else {
            indirect.push(card);
        }
    }
    // Different editions of one root remain separate immutable accounts.
    let cards = interleave(lexical_cards, indirect)
        .into_iter()
        .enumerate()
        .map(|(index, card)| {
            card.with_rank(NonZeroU16::new((index + 1) as u16).expect("bounded episode rank"))
        })
        .collect();
    Ok((
        coverage,
        LaneWindow::bounded(cards, lexical.further_tail_unknown()),
    ))
}

// Touchstone reference metadata is independent of learned association edges.
// Only the frozen original anchors are queried; an incoming owner is hydrated
// for its view, but never added to the discovery queue.
trait TouchstoneReader: Sync {
    fn catalog(&self) -> impl Future<Output = Result<TouchstonePage, Error>> + Send;
    fn referrers(
        &self,
        id: NodeId,
        after: Option<TouchstoneCursor>,
    ) -> impl Future<Output = Result<TouchstonePage, Error>> + Send;
    fn record(
        &self,
        id: NodeId,
    ) -> impl Future<Output = Result<Option<TouchstoneRecord>, Error>> + Send;
    fn snapshot(
        &self,
        id: NodeId,
    ) -> impl Future<Output = Result<Option<SummarySnapshot>, Error>> + Send;
}
impl TouchstoneReader for Memory {
    async fn catalog(&self) -> Result<TouchstonePage, Error> {
        self.touchstones_page(&TouchstonePageRequest::new(None, None, 1)?)
            .await
    }
    async fn referrers(
        &self,
        id: NodeId,
        after: Option<TouchstoneCursor>,
    ) -> Result<TouchstonePage, Error> {
        self.touchstone_referrers_page(&TouchstoneReferrersRequest::new(id, after, 1)?)
            .await
    }
    async fn record(&self, id: NodeId) -> Result<Option<TouchstoneRecord>, Error> {
        self.get_touchstone(id).await
    }
    async fn snapshot(&self, id: NodeId) -> Result<Option<SummarySnapshot>, Error> {
        self.summary_snapshot(id).await
    }
}

struct TouchstoneWindow {
    views: BTreeMap<NodeId, TouchstoneView>,
    incoming: Vec<PrimaryInputCard>,
    coverage: TouchstoneRetrieval,
}

fn touchstone_error(error: &Error) -> TouchstoneStopReason {
    match error {
        Error::InvalidInput(message)
            if message == "native touchstone operations are unsupported by this store" =>
        {
            TouchstoneStopReason::Unsupported
        }
        _ => TouchstoneStopReason::ReadError,
    }
}

async fn touchstone_read<T>(
    deadline: Instant,
    future: impl Future<Output = Result<T, Error>>,
) -> Result<T, TouchstoneStopReason> {
    let duration = remaining(deadline).ok_or(TouchstoneStopReason::Deadline)?;
    match tokio::time::timeout(duration, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(touchstone_error(&error)),
        Err(_) => Err(TouchstoneStopReason::Deadline),
    }
}

fn admit_touchstone_read(coverage: &mut TouchstoneRetrieval, deadline: Instant) -> bool {
    let reason = if remaining(deadline).is_none() {
        Some(TouchstoneStopReason::Deadline)
    } else if coverage.reads() >= coverage.read_limit {
        Some(TouchstoneStopReason::Budget)
    } else {
        None
    };
    if let Some(reason) = reason {
        coverage.stop_reason.get_or_insert(reason);
        coverage.further_tail_unknown = true;
        false
    } else {
        true
    }
}

async fn project_touchstone(
    reader: &impl TouchstoneReader,
    record: &TouchstoneRecord,
    original: &BTreeSet<NodeId>,
    direct: bool,
    origin: TouchstoneOrigin,
    coverage: &mut TouchstoneRetrieval,
    health: &mut BTreeMap<NodeId, Option<SummarySnapshot>>,
    deadline: Instant,
) -> Result<TouchstoneView, ContextError> {
    // Leave room for the author's own note, rather than reserving the full
    // immutable record. Large subjects/origin unions still pack atomically.
    const COMPACT_REFERENCE_BYTES: usize = 1024;
    let total = u32::try_from(record.references().len())
        .map_err(|_| Error::Backend("touchstone reference collection exceeds u32".into()))?;
    let mut view = TouchstoneView::new(
        record.subject().as_str().to_owned(),
        Vec::new(),
        total,
        vec![origin],
    )?;
    for snapshot in record.references() {
        if !direct && !original.contains(&snapshot.id()) {
            continue;
        }
        let mut row = TouchstoneReferenceView::new(
            NodeId(snapshot.db_id()),
            snapshot.id(),
            snapshot.digest().to_hex(),
            snapshot.summary().as_str(),
            128,
            TouchstoneResolution::Unavailable,
        )?;
        // Check whole-row feasibility before spending optional health work.
        let mut trial = view.clone();
        trial.references.push(row.clone());
        trial.references_omitted -= 1;
        if serde_json::to_vec(&trial)
            .expect("typed touchstone view encodes")
            .len()
            > COMPACT_REFERENCE_BYTES
        {
            break;
        }
        if let Some(current) = health.get(&snapshot.id()) {
            row.resolution = match current {
                Some(current) if current.digest() == snapshot.digest() => {
                    TouchstoneResolution::MatchesSnapshot
                }
                Some(_) => TouchstoneResolution::ChangedSnapshot,
                None => TouchstoneResolution::Missing,
            };
        } else if admit_touchstone_read(coverage, deadline) {
            coverage.target_reads += 1;
            match touchstone_read(deadline, reader.snapshot(snapshot.id())).await {
                Ok(current) => {
                    row.resolution = match &current {
                        Some(current) if current.digest() == snapshot.digest() => {
                            TouchstoneResolution::MatchesSnapshot
                        }
                        Some(_) => TouchstoneResolution::ChangedSnapshot,
                        None => TouchstoneResolution::Missing,
                    };
                    health.insert(snapshot.id(), current);
                }
                Err(reason) => {
                    coverage.stop_reason.get_or_insert(reason);
                    coverage.further_tail_unknown = true;
                }
            }
        }
        // Resolution spellings have different sizes: final exact row, not the
        // provisional unavailable spelling, decides whole-row admission.
        trial.references.pop();
        trial.references.push(row);
        if serde_json::to_vec(&trial)
            .expect("typed touchstone view encodes")
            .len()
            > COMPACT_REFERENCE_BYTES
        {
            break;
        }
        view = trial;
    }
    Ok(view)
}

async fn compose_touchstones(
    reader: &impl TouchstoneReader,
    frozen: &[NodeId],
    semantic: &[NodeId],
    read_limit: usize,
    deadline: Instant,
) -> Result<TouchstoneWindow, ContextError> {
    let mut output = TouchstoneWindow {
        views: BTreeMap::new(),
        incoming: Vec::new(),
        coverage: TouchstoneRetrieval::default(),
    };
    if frozen.is_empty() || read_limit == 0 {
        return Ok(output);
    }
    let original = frozen.iter().copied().collect::<BTreeSet<_>>();
    let direct = semantic.iter().copied().collect::<BTreeSet<_>>();
    output.coverage.searched = true;
    output.coverage.read_limit = read_limit.min(2 * MAX_LINKED_CAPACITY) as u32;
    output.coverage.anchors_total = original.len() as u32;
    if !admit_touchstone_read(&mut output.coverage, deadline) {
        return Ok(output);
    }
    output.coverage.catalog_reads += 1;
    let catalog = match touchstone_read(deadline, reader.catalog()).await {
        Ok(page) => page,
        Err(reason) => {
            output.coverage.stop_reason = Some(reason);
            output.coverage.further_tail_unknown = true;
            return Ok(output);
        }
    };
    if catalog.items.len() > 1 {
        return Err(Error::Backend("touchstone catalog exceeded one-row request".into()).into());
    }
    // One indexed empty catalog observation avoids N empty owner probes on
    // ordinary graphs. It is not a corpus snapshot lasting beyond this read.
    if catalog.items.is_empty() && catalog.next.is_none() {
        return Ok(output);
    }
    enum Work {
        Owner(NodeId),
        Referrers(NodeId, Option<TouchstoneCursor>),
    }
    let mut work = VecDeque::new();
    let mut queued = BTreeSet::new();
    for &id in frozen {
        if !queued.insert(id) {
            continue;
        }
        if direct.contains(&id) {
            work.push_back(Work::Owner(id));
        }
        work.push_back(Work::Referrers(id, None));
    }
    let mut records = BTreeMap::<NodeId, Option<TouchstoneRecord>>::new();
    let mut health = BTreeMap::new();
    let mut examined = BTreeSet::new();
    let mut incoming_indices = BTreeMap::<NodeId, usize>::new();
    while let Some(job) = work.pop_front() {
        if !admit_touchstone_read(&mut output.coverage, deadline) {
            break;
        }
        match job {
            Work::Owner(id) => {
                examined.insert(id);
                output.coverage.record_reads += 1;
                match touchstone_read(deadline, reader.record(id)).await {
                    Ok(Some(record)) => {
                        let view = project_touchstone(
                            reader,
                            &record,
                            &original,
                            true,
                            TouchstoneOrigin::Direct,
                            &mut output.coverage,
                            &mut health,
                            deadline,
                        )
                        .await?;
                        output.views.insert(id, view);
                        records.insert(id, Some(record));
                    }
                    Ok(None) => {
                        records.insert(id, None);
                    }
                    Err(reason) => {
                        output.coverage.stop_reason.get_or_insert(reason);
                        output.coverage.further_tail_unknown = true;
                        break;
                    }
                }
            }
            Work::Referrers(id, after) => {
                examined.insert(id);
                output.coverage.referrer_page_reads += 1;
                let page =
                    match touchstone_read(deadline, reader.referrers(id, after.clone())).await {
                        Ok(page) => page,
                        Err(reason) => {
                            output.coverage.stop_reason.get_or_insert(reason);
                            output.coverage.further_tail_unknown = true;
                            break;
                        }
                    };
                if page.items.len() > 1
                    || page
                        .next
                        .as_ref()
                        .is_some_and(|next| after.as_ref() == Some(next))
                {
                    return Err(Error::Backend(
                        "touchstone referrer page exceeded bounds or did not advance".into(),
                    )
                    .into());
                }
                for header in page.items {
                    // Archive is not broken reference health. Ordinary semantic
                    // recall still selects active owners; browse retains others.
                    if header.status != mneme_core::NodeStatus::Active {
                        continue;
                    }
                    let owner = header.id;
                    let origin = TouchstoneOrigin::Referrer { anchor_id: id };
                    if let Some(view) = output.views.get_mut(&owner) {
                        view.origins.push(origin);
                        *view = TouchstoneView::new(
                            view.subject.clone(),
                            view.references.clone(),
                            view.references_omitted,
                            view.origins.clone(),
                        )?;
                    } else {
                        let record = if let Some(record) = records.get(&owner) {
                            record.clone()
                        } else {
                            if !admit_touchstone_read(&mut output.coverage, deadline) {
                                break;
                            }
                            output.coverage.record_reads += 1;
                            let record = match touchstone_read(deadline, reader.record(owner)).await
                            {
                                Ok(record) => record,
                                Err(reason) => {
                                    output.coverage.stop_reason.get_or_insert(reason);
                                    output.coverage.further_tail_unknown = true;
                                    break;
                                }
                            };
                            records.insert(owner, record.clone());
                            record
                        };
                        let Some(record) = record else {
                            output.coverage.further_tail_unknown = true;
                            continue;
                        };
                        if record.owner() != owner || record.subject() != &header.subject {
                            return Err(Error::Backend(
                                "touchstone referrer record identity mismatch".into(),
                            )
                            .into());
                        }
                        let mut view = project_touchstone(
                            reader,
                            &record,
                            &original,
                            direct.contains(&owner),
                            origin,
                            &mut output.coverage,
                            &mut health,
                            deadline,
                        )
                        .await?;
                        if direct.contains(&owner) {
                            view.origins.push(TouchstoneOrigin::Direct);
                            view = TouchstoneView::new(
                                view.subject.clone(),
                                view.references.clone(),
                                view.references_omitted,
                                view.origins,
                            )?;
                        }
                        output.views.insert(owner, view);
                    }
                    if !direct.contains(&owner) {
                        if let Some(&index) = incoming_indices.get(&owner) {
                            output.incoming[index] = PrimaryInputCard::new(
                                owner,
                                NonZeroU16::new(1).unwrap(),
                                header.summary.as_str(),
                            )?
                            .with_touchstone(output.views[&owner].clone());
                        } else {
                            incoming_indices.insert(owner, output.incoming.len());
                            output.incoming.push(
                                PrimaryInputCard::new(
                                    owner,
                                    NonZeroU16::new(1).unwrap(),
                                    header.summary.as_str(),
                                )?
                                .with_touchstone(output.views[&owner].clone()),
                            );
                            output.coverage.owners_discovered += 1;
                        }
                    }
                }
                if let Some(next) = page.next {
                    work.push_back(Work::Referrers(id, Some(next)));
                }
            }
        }
        if output.coverage.stop_reason.is_some() {
            break;
        }
    }
    output.coverage.anchors_examined = examined.len() as u32;
    output.coverage.further_tail_unknown |= !work.is_empty();
    output.coverage.validate()?;
    Ok(output)
}

/// Adapt the ordinary query envelope to the deliberately smaller plain-text
/// lexical cue contract without silently changing the semantic query itself.
fn normalize_episode_cue(text: &str) -> Option<(EpisodeCue, bool, bool)> {
    let whitespace_normalized = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if whitespace_normalized.is_empty() {
        return None;
    }
    let normalized = whitespace_normalized != text;
    let truncated = whitespace_normalized.len() > MAX_EPISODE_CUE_BYTES;
    let mut end = whitespace_normalized.len().min(MAX_EPISODE_CUE_BYTES);
    while !whitespace_normalized.is_char_boundary(end) {
        end -= 1;
    }
    let cue = EpisodeCue::new(whitespace_normalized[..end].trim_end())
        .expect("normalized nonempty cue is within the checked UTF-8 bound");
    Some((cue, normalized, truncated))
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "context_touchstone_tests.rs"]
mod touchstone_tests;

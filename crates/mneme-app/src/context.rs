//! Ordinary recall combines semantic knowledge and an explicitly episodic lane.
//! Retrieval policies stay separate; one presenter owns their shared context budget.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU16,
    time::Instant,
};

use mneme_core::NodeId;
use mneme_core::episode::{
    EpisodeCue, EpisodeCuePage, EpisodeCueRequest, EpisodeFilter, EpisodePageLimit,
    EpisodeUnavailableReason, MAX_EPISODE_CUE_BYTES, MAX_EPISODE_PAGE_ITEMS,
};
use mneme_core::ports::{
    Budget, Error, RoutingBinding, RoutingDiagnostics, RoutingHint, RoutingOutcome, StatusFilter,
    TraversalHop,
};
use mneme_engine::Memory;
use mneme_present::{
    CoreInputCard, EpisodeInputCard, EpisodicRetrieval, EpisodicUnavailableReason,
    ExpansionInputCard, Lane, LaneWindow, PackPlan, PackingInput, PresentationBudget,
    PrimaryInputCard, pack,
};

mod discovery;
mod linked_scenes;
mod touchstones;

use discovery::{LINKED_READ_TIMEOUT, MAX_LINKED_CAPACITY, interleave};
use linked_scenes::compose_linked_scenes;
use touchstones::compose_touchstones;

use crate::{ContextError, presentation_retrieval_metadata};

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
            let mut card = PrimaryInputCard::new(
                hit.node.id(),
                hit.node.created(),
                hit.lane_rank,
                hit.node.summary(),
            )?;
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
mod test_support;

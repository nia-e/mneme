//! One-hop exact-edition scene discovery. The reader seam, endpoint cache,
//! round-robin scheduler and work accounting have one owner here; discovered
//! scenes never become new anchors and historical editions never become heads.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::Future,
    num::NonZeroU16,
    time::{Duration, Instant},
};

use mneme_core::NodeId;
use mneme_core::episode::{EpisodeHeader, EpisodeHeaderByEdition, EpisodeHeaderByEditionRequest};
use mneme_core::ports::{Error, IncidentEdgesCursor, IncidentEdgesPage, IncidentEdgesRequest};
use mneme_engine::Memory;
use mneme_present::{
    EpisodeAnchor, EpisodeInputCard, EpisodeReferenceCounts, EpisodeReferenceRetrieval,
    EpisodeReferenceStopReason, LaneWindow,
};

use super::discovery::{LINKED_READ_TIMEOUT, MAX_LINKED_CAPACITY, interleave, remaining};
use crate::ContextError;

/// Narrow query-local read seam: no mutation, semantic neighbours, body fetch,
/// feedback receipt or recursive retrieval is available to this composer.
pub(super) trait LinkedSceneReader: Sync {
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

pub(super) async fn compose_linked_scenes(
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

#[cfg(test)]
mod tests;

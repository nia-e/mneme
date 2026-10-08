//! Grounded feedback and exact walk-receipt observations.
//!
//! This module owns feedback validation, canonical retry identity, and atomic
//! node/edge training through `Memory`. The public facade remains at the crate
//! root; shared hydration and mutation coordination stay with `Memory`.

use std::collections::{BTreeMap, HashMap, HashSet};

use mneme_core::ports::{
    Error, FeedbackCommit, FeedbackCommitOutcome, FeedbackEdgeUpdate, FeedbackIdempotency,
    FeedbackMergeObservation, FeedbackNodeUpdate, FeedbackRetryScope, Result,
};
use mneme_core::{Edge, EdgeKind, MAX_FEEDBACK_BATCH_EVENTS, Node, NodeId, Signal};

use crate::Memory;

/// The exact stored edge used for one first-visit walk hop. `previous` and
/// `target` describe the movement; `edge_from` and `edge_to` preserve the stored
/// arrow even when the hop followed an incoming edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObservedRoute {
    pub previous: NodeId,
    pub target: NodeId,
    pub edge_from: NodeId,
    pub edge_to: NodeId,
}

impl ObservedRoute {
    pub fn new(
        previous: NodeId,
        target: NodeId,
        edge_from: NodeId,
        edge_to: NodeId,
    ) -> Result<Self> {
        let route = Self {
            previous,
            target,
            edge_from,
            edge_to,
        };
        route.validate()?;
        Ok(route)
    }

    fn validate(&self) -> Result<()> {
        if self.previous == self.target || self.edge_from == self.edge_to {
            return Err(Error::InvalidInput(
                "walk receipt route cannot be a self-loop".into(),
            ));
        }
        let follows_stored_arrow = self.edge_from == self.previous && self.edge_to == self.target;
        let follows_incoming_arrow = self.edge_from == self.target && self.edge_to == self.previous;
        if !follows_stored_arrow && !follows_incoming_arrow {
            return Err(Error::InvalidInput(
                "walk receipt route edge does not connect previous and target".into(),
            ));
        }
        Ok(())
    }

    /// Whether the walk moved against the stored edge arrow.
    pub fn incoming(&self) -> bool {
        self.edge_to == self.previous
    }
}

/// One observation captured by an opaque walk receipt. A routed observation
/// records the exact stored edge identity; a `None` route means an observed
/// node (normally a walk start) that received no incoming hop. `relevant` is an
/// explicit positive or negative judgment; unknown observations emit no event.
/// The trusted receipt owner validates observed membership and disjoint judgments.
/// `NotNew` remains a separate direct-feedback judgment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiptFeedback {
    pub target: NodeId,
    pub route: Option<ObservedRoute>,
    pub relevant: bool,
}

impl ReceiptFeedback {
    pub fn routed(route: ObservedRoute, relevant: bool) -> Self {
        Self {
            target: route.target,
            route: Some(route),
            relevant,
        }
    }

    pub fn node_used(target: NodeId) -> Self {
        Self {
            target,
            route: None,
            relevant: true,
        }
    }

    /// Explicit negative judgment on an observed node without an incoming route.
    pub fn node_unhelpful(target: NodeId) -> Self {
        Self {
            target,
            route: None,
            relevant: false,
        }
    }

    fn validate(&self) -> Result<()> {
        if let Some(route) = self.route {
            route.validate()?;
            if route.target != self.target {
                return Err(Error::InvalidInput(
                    "walk receipt event target does not match its observed route".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Result of one atomic receipt-feedback batch. Counts describe the validated
/// receipt payload, including a safe replay; `commit` tells observability code
/// whether this call performed the write or found the same durable operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiptFeedbackResult {
    pub commit: FeedbackCommitOutcome,
    pub reinforced: usize,
    pub interfered: usize,
}

impl Memory {
    /// Apply an agent's feedback about a node it just loaded, *relative to the
    /// node it arrived from* (`prior`). This is the explicit signal that drives
    /// edge weight — the strong counterpart to retrieval's automatic prior:
    ///
    /// - [`Signal::RelevantNew`] — record grounded node use and reinforce the
    ///   `prior → target` edge, creating it if needed.
    /// - [`Signal::NotNew`] — the same, plus bank a [`MergeCandidate`](mneme_core::MergeCandidate).
    /// - [`Signal::Irrelevant`] — bank interference on the observed edge only.
    ///
    /// With no `prior`, positive feedback records grounded use; negative
    /// feedback changes no persistent node or edge state. Resilient by design: skipping feedback just means no
    /// signal — nothing is corrupted. All graph effects share one adapter commit
    /// point. This compatibility API has no caller operation id, however, so a
    /// retry after an ambiguous post-commit transport failure can apply the
    /// signal twice; receipt feedback is the idempotent network-facing path.
    pub async fn apply_feedback(
        &self,
        prior: Option<NodeId>,
        target: NodeId,
        signal: Signal,
    ) -> Result<()> {
        let _mutation = self.mutation_gate.lock().await;
        let now = self.clock.now();
        let Some(mut node) = self.graph.get_node(target).await? else {
            // Walk receipts and UI results can outlive an explicit forget. Stale
            // feedback is harmless, but must never recreate an edge to the gone
            // node.
            return Ok(());
        };
        Self::require_semantic(&node)?;
        let prior = match prior.filter(|prior| *prior != target) {
            Some(prior) => match self.graph.get_node(prior).await? {
                Some(prior_node) => {
                    Self::require_semantic(&prior_node)?;
                    Some(prior)
                }
                None => None,
            },
            None => None,
        };
        let expected_node = node.clone();
        let mut edge_update = None;
        let mut merge_observations = Vec::new();
        match signal {
            Signal::RelevantNew | Signal::NotNew => {
                node.record_grounded_use(now);
                if let Some(prior) = prior {
                    // Asymmetric: strengthen the *forward* `prior -> target`
                    // transition (the direction actually traversed-and-relevant),
                    // creating a forward-only edge when the pair is new.
                    let expected = self.graph.get_edge(prior, target).await?;
                    let mut replacement = expected.clone().unwrap_or_else(|| {
                        Edge::new(prior, target, 0.0, EdgeKind::Transition, now)
                    });
                    replacement.reinforce(now, self.strength_for(replacement.kind));
                    edge_update = Some(FeedbackEdgeUpdate {
                        expected,
                        replacement,
                    });
                    if signal == Signal::NotNew {
                        merge_observations.push(FeedbackMergeObservation::new(prior, target)?);
                    }
                }
            }
            Signal::Irrelevant => {
                // Weaken whichever directed edge surfaced `target` for `prior`.
                if let Some(prior) = prior
                    && let Some(mut edge) = self.connecting_edge(prior, target).await?
                {
                    let expected = edge.clone();
                    edge.mark_interference();
                    edge_update = Some(FeedbackEdgeUpdate {
                        expected: Some(expected),
                        replacement: edge,
                    });
                }
            }
        }

        let commit = FeedbackCommit {
            idempotency: None,
            applied_at: now,
            nodes: if signal == Signal::Irrelevant {
                Vec::new()
            } else {
                vec![FeedbackNodeUpdate {
                    expected: expected_node,
                    replacement: node,
                }]
            },
            edges: edge_update.into_iter().collect(),
            merge_observations,
        };
        commit.validate()?;
        self.graph.commit_feedback(&commit).await?;
        Ok(())
    }

    /// Atomically apply every edge observation carried by one or more completed
    /// walk receipts. Unlike looping over [`Self::apply_feedback`], an adapter
    /// error cannot leave a prefix trained. This unkeyed form is useful for a
    /// local in-process walk whose caller already owns exactly-once control; a
    /// network receipt should use [`Self::apply_receipt_feedback_idempotent`].
    pub async fn apply_receipt_feedback(
        &self,
        events: &[ReceiptFeedback],
    ) -> Result<ReceiptFeedbackResult> {
        self.apply_receipt_feedback_inner(None, events).await
    }

    /// Atomic receipt feedback with durable at-most-once retry semantics. The
    /// trusted host derives `key` canonically from the claimed receipt tokens and
    /// must reuse it for every retry. The engine binds it to an exact ordered
    /// event fingerprint; same key/same payload is a successful no-op, while the
    /// same key with another payload fails closed. `retry` is valid only for the
    /// lifetime of the exclusive host generation that issued the receipts; a
    /// server restart intentionally begins a new reachability epoch.
    pub async fn apply_receipt_feedback_idempotent(
        &self,
        key: &str,
        retry: &FeedbackRetryScope,
        events: &[ReceiptFeedback],
    ) -> Result<ReceiptFeedbackResult> {
        self.apply_receipt_feedback_inner(Some((key, retry)), events)
            .await
    }

    async fn apply_receipt_feedback_inner(
        &self,
        idempotency: Option<(&str, &FeedbackRetryScope)>,
        events: &[ReceiptFeedback],
    ) -> Result<ReceiptFeedbackResult> {
        if events.len() > MAX_FEEDBACK_BATCH_EVENTS {
            return Err(Error::CapacityExceeded {
                resource: "feedback batch events",
                limit: MAX_FEEDBACK_BATCH_EVENTS,
            });
        }
        let events = canonical_receipt_feedback(events)?;
        let fingerprint = receipt_feedback_fingerprint(&events);
        let idempotency = idempotency
            .map(|(key, retry)| FeedbackIdempotency::new(key, fingerprint.clone(), retry.clone()))
            .transpose()?;
        let reinforced = events.iter().filter(|event| event.relevant).count();
        let interfered = events.len() - reinforced;

        let _mutation = self.mutation_gate.lock().await;
        let now = self.clock.now();
        // Cache both positive and negative reads. A node receives one judgment
        // per batch, even when distinct observed routes reached it. Each final
        // row appears only once in the compare-and-commit delta.
        // Admit every endpoint before processing events, even when its partner
        // was forgotten. A stale route through an episode must not let an
        // otherwise valid prefix of the receipt train semantic memory.
        let mut ids: Vec<_> = events
            .iter()
            .flat_map(|event| {
                std::iter::once(event.target).chain(event.route.map(|route| route.previous))
            })
            .collect();
        ids.sort_unstable();
        ids.dedup();
        let hydrated = self.hydrate_nodes(&ids).await?;
        let mut nodes: BTreeMap<NodeId, Option<Node>> = ids.into_iter().zip(hydrated).collect();
        let mut expected_nodes = BTreeMap::new();
        for (id, node) in &nodes {
            if let Some(node) = node {
                Self::require_semantic(node)?;
                expected_nodes.insert(*id, node.clone());
            }
        }
        let mut changed_nodes = HashSet::new();
        let mut edges: BTreeMap<(NodeId, NodeId), Option<Edge>> = BTreeMap::new();
        let mut expected_edges: BTreeMap<(NodeId, NodeId), Option<Edge>> = BTreeMap::new();
        let mut changed_edges = HashSet::new();
        // Several walks can observe the same stored arrow. Count it once per
        // batch; opposing explicit judgments make that route attribution unknown,
        // without discarding either endpoint's node-level judgment.
        let mut edge_judgments: HashMap<(NodeId, NodeId), Option<bool>> = HashMap::new();
        for event in &events {
            if let Some(route) = event.route {
                edge_judgments
                    .entry((route.edge_from, route.edge_to))
                    .and_modify(|judgment| {
                        if *judgment != Some(event.relevant) {
                            *judgment = None;
                        }
                    })
                    .or_insert(Some(event.relevant));
            }
        }

        for event in &events {
            let Some(node) = nodes.get_mut(&event.target).and_then(Option::as_mut) else {
                // A receipt can outlive explicit forget. Preserve the existing
                // harmless semantics without recreating either endpoint.
                continue;
            };
            // Lifecycle can change after a route was observed. The used node still
            // deserves its grounded node-level signal, but an archived endpoint
            // must not keep zombie topology strong through an old receipt.
            let target_is_archived = node.is_archived();
            if event.relevant && changed_nodes.insert(event.target) {
                node.record_grounded_use(now);
            }

            let Some(route) = event.route else {
                continue;
            };

            let Some(previous) = nodes.get(&route.previous).and_then(Option::as_ref) else {
                continue;
            };

            // Negative feedback may still weaken a stale route, but positive
            // receipt feedback cannot reinforce an edge whose source or target
            // has been archived since the walk observed it. Node feedback above
            // remains intact as grounded-use telemetry.
            if event.relevant && (target_is_archived || previous.is_archived()) {
                continue;
            }

            // Receipts train only the exact stored edge that the walk observed.
            // If it disappeared after traversal, keep the node feedback but do
            // not recreate the route or fall back to the opposite arrow. Treat a
            // now-incompatible edge kind the same way: future library callers or
            // corrupt state cannot train a hop traversal would not currently show.
            let identity = (route.edge_from, route.edge_to);
            if changed_edges.contains(&identity)
                || edge_judgments.get(&identity) != Some(&Some(event.relevant))
            {
                continue;
            }
            if let std::collections::btree_map::Entry::Vacant(entry) = edges.entry(identity) {
                let current = self.graph.get_edge(identity.0, identity.1).await?;
                expected_edges.insert(identity, current.clone());
                entry.insert(current);
            }
            let Some(edge) = edges.get_mut(&identity).and_then(Option::as_mut) else {
                continue;
            };
            let still_traversable = if route.incoming() {
                edge.kind.traverses_incoming()
            } else {
                edge.kind.traverses_outgoing()
            };
            if !still_traversable {
                continue;
            }
            if event.relevant {
                edge.reinforce(now, self.strength_for(edge.kind));
            } else {
                edge.mark_interference();
            }
            changed_edges.insert(identity);
        }

        let node_updates = changed_nodes
            .into_iter()
            .map(|id| FeedbackNodeUpdate {
                expected: expected_nodes
                    .remove(&id)
                    .expect("a changed node had an initial value"),
                replacement: nodes
                    .remove(&id)
                    .flatten()
                    .expect("a changed node remains present"),
            })
            .collect();
        let edge_updates = changed_edges
            .into_iter()
            .map(|identity| FeedbackEdgeUpdate {
                expected: expected_edges
                    .remove(&identity)
                    .expect("a changed edge was initially cached"),
                replacement: edges
                    .remove(&identity)
                    .flatten()
                    .expect("a changed edge remains present"),
            })
            .collect();
        let commit = FeedbackCommit {
            idempotency,
            applied_at: now,
            nodes: node_updates,
            edges: edge_updates,
            merge_observations: Vec::new(),
        };
        commit.validate()?;
        let commit = self.graph.commit_feedback(&commit).await?;
        Ok(ReceiptFeedbackResult {
            commit,
            reinforced,
            interfered,
        })
    }
}

/// Normalize one receipt batch before fingerprinting or mutation. Receipt order
/// and duplicate observations cannot multiply credit; opposed node judgments are
/// invalid rather than an order-dependent update.
fn canonical_receipt_feedback(events: &[ReceiptFeedback]) -> Result<Vec<ReceiptFeedback>> {
    let mut judgments = HashMap::new();
    for event in events {
        event.validate()?;
        if judgments
            .insert(event.target, event.relevant)
            .is_some_and(|previous| previous != event.relevant)
        {
            return Err(Error::InvalidInput(
                "receipt feedback node cannot be both used and unhelpful".into(),
            ));
        }
    }
    let mut events = events.to_vec();
    events.sort_unstable_by_key(|event| {
        (
            event.target,
            event
                .route
                .map(|route| (route.previous, route.edge_from, route.edge_to)),
            event.relevant,
        )
    });
    events.dedup();
    Ok(events)
}

/// Domain-separated SHA-256 identity of one canonical ordered feedback batch.
/// Keeping only the fixed digest makes retry-ledger storage independent of the
/// number of reached edges while preserving practical collision resistance.
fn receipt_feedback_fingerprint(events: &[ReceiptFeedback]) -> String {
    use sha2::{Digest, Sha256};

    let mut hash = Sha256::new();
    hash.update(b"mneme-receipt-feedback-v4-explicit\0");
    hash.update((events.len() as u64).to_be_bytes());
    for event in events {
        hash.update(event.target.0.to_bytes());
        match event.route {
            Some(route) => {
                hash.update([1]);
                hash.update(route.previous.0.to_bytes());
                hash.update(route.edge_from.0.to_bytes());
                hash.update(route.edge_to.0.to_bytes());
            }
            None => hash.update([0]),
        }
        hash.update([u8::from(event.relevant)]);
    }
    format!("{:x}", hash.finalize())
}

#[cfg(test)]
mod tests {
    use mneme_core::NodeId;
    use ulid::Ulid;

    use super::{
        ObservedRoute, ReceiptFeedback, canonical_receipt_feedback, receipt_feedback_fingerprint,
    };

    #[test]
    fn explicit_negative_start_is_valid_but_opposed_node_judgments_are_not() {
        let node = NodeId(Ulid::from(1u128));
        assert!(canonical_receipt_feedback(&[ReceiptFeedback::node_unhelpful(node)]).is_ok());
        assert!(
            canonical_receipt_feedback(&[
                ReceiptFeedback::node_used(node),
                ReceiptFeedback::node_unhelpful(node),
            ])
            .is_err()
        );
    }

    #[test]
    fn canonical_feedback_deduplicates_and_ignores_input_order() {
        let a = ReceiptFeedback::node_used(NodeId(Ulid::from(1u128)));
        let b = ReceiptFeedback::node_unhelpful(NodeId(Ulid::from(2u128)));
        let first = canonical_receipt_feedback(&[a, b]).unwrap();
        let reordered = canonical_receipt_feedback(&[b, a, b, a]).unwrap();
        assert_eq!(first, reordered);
        assert_eq!(
            receipt_feedback_fingerprint(&first),
            receipt_feedback_fingerprint(&reordered)
        );
        assert_ne!(
            receipt_feedback_fingerprint(&first),
            receipt_feedback_fingerprint(&[])
        );
    }

    #[test]
    fn retry_fingerprint_preserves_v4_identity_for_node_and_directed_routes() {
        let a = NodeId(Ulid::from(1u128));
        let b = NodeId(Ulid::from(2u128));
        let c = NodeId(Ulid::from(3u128));
        let start = ReceiptFeedback::node_unhelpful(a);
        let outgoing = ReceiptFeedback::routed(ObservedRoute::new(a, b, a, b).unwrap(), true);
        let incoming = ReceiptFeedback::routed(ObservedRoute::new(b, c, c, b).unwrap(), true);
        let events = canonical_receipt_feedback(&[incoming, start, outgoing, incoming]).unwrap();

        // Persisted retry ledgers keep this digest, not the request itself.
        // Refactoring must retain node judgments and exact stored-arrow identity.
        assert_eq!(
            receipt_feedback_fingerprint(&events),
            "22134160c0caa34e78baca491d73af8cfa7e548670e14d26cd351727408dbe97"
        );
    }
}

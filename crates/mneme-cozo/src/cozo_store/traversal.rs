use super::*;

#[cfg(test)]
mod provenance_tests {
    use crate::routing_probe::RoutingBiasMap;

    #[async_trait::async_trait]
    impl crate::tests::RoutingProbeStore for super::CozoStore {
        async fn probe_spread(
            &self,
            seeds: &[mneme_core::ports::Scored],
            budget: mneme_core::ports::Budget,
            query: Option<&[f32]>,
            scope: mneme_core::ports::TraversalScope,
            biases: Option<&RoutingBiasMap>,
        ) -> mneme_core::ports::Result<mneme_core::ports::SpreadResult> {
            self.spread_inner(seeds, budget, query, scope, true, biases)
                .await
        }
    }

    #[tokio::test]
    async fn cozo_spread_provenance_preserves_actual_winning_paths() {
        crate::tests::assert_spread_provenance(&super::CozoStore::new(4).unwrap()).await;
    }

    #[tokio::test]
    async fn cozo_signed_routing_probe_changes_both_native_cutoffs() {
        crate::tests::assert_signed_routing_probe(&super::CozoStore::new(4).unwrap()).await;
    }
    #[tokio::test]
    async fn cozo_routing_ordering_winners_survive_small_caps() {
        crate::tests::assert_routing_ordering_winners(&super::CozoStore::new(4).unwrap()).await;
    }
}

impl CozoStore {
    async fn spread_inner(
        &self,
        seeds: &[Scored],
        budget: Budget,
        query: Option<&[f32]>,
        scope: TraversalScope,
        trace: bool,
        routing_biases: Option<&crate::routing_probe::RoutingBiasMap>,
    ) -> Result<mneme_core::ports::SpreadResult> {
        let active_ordering = routing_biases.is_some_and(|biases| !biases.is_empty());
        let mut ordering = active_ordering.then(BTreeMap::new);
        let mut paths = (trace || active_ordering).then(BTreeMap::new);
        if budget.max_nodes == 0 {
            return Ok(mneme_core::ports::SpreadResult {
                hits: Vec::new(),
                paths: trace.then_some(paths).flatten(),
                ordering,
            });
        }
        let gamma = budget.query_conditioning;
        if gamma > 0.0
            && let Some(query) = query
            && query.len() != self.dim
        {
            return Err(Error::DimMismatch {
                index: self.dim,
                provider: query.len(),
            });
        }

        // Validate all seeds in one indexed status query. This makes the scope a
        // real traversal boundary even for direct port callers, rather than an
        // assumption that only the engine happens to satisfy.
        let mut seed_ids: Vec<NodeId> = seeds.iter().map(|seed| seed.id).collect();
        seed_ids.sort();
        seed_ids.dedup();
        let seed_statuses = self.statuses_for(&seed_ids).await?;

        let mut seed_scores: HashMap<NodeId, f32> = HashMap::new();
        for s in seeds {
            if !seed_statuses
                .get(&s.id)
                .is_some_and(|status| scope.allows(s.id, *status))
            {
                continue;
            }
            let a = s.score.max(0.0);
            crate::propose_score(&mut seed_scores, s.id, a);
        }
        let admitted_seeds = crate::bounded_ranked(seed_scores, budget.max_nodes);
        let mut best: HashMap<NodeId, f32> = admitted_seeds
            .iter()
            .map(|seed| (seed.id, seed.score))
            .collect();
        let mut frontier = best.clone();

        let mut expanded_sources = 0usize;
        for _depth in 0..budget.max_depth {
            if frontier.is_empty() || expanded_sources >= budget.max_nodes {
                break;
            }

            let mut sources: Vec<Scored> = frontier
                .drain()
                .map(|(id, score)| Scored { id, score })
                .collect();
            sources.sort_by(|a, b| crate::routed_scored_order(a, b, ordering.as_ref()));
            sources.truncate(budget.max_nodes - expanded_sources);
            expanded_sources += sources.len();
            let source_ids: Vec<NodeId> = sources.iter().map(|source| source.id).collect();
            let conditioned = gamma > 0.0 && query.is_some();
            let candidate_cap = if conditioned {
                SPREAD_CONDITION_CANDIDATES
            } else {
                SPREAD_FANOUT
            };
            let mut layer = self
                .scoped_layer(&source_ids, scope, candidate_cap, routing_biases)
                .await?;

            // Query-conditioning is exact over the bounded layer: fetch only the
            // at-most `frontier × SPREAD_CONDITION_CANDIDATES` vectors that can
            // compete for the final fanout. This avoids both a whole-graph scan
            // and choosing the final eight by raw edge weight before relevance is
            // known.
            let conditioning = if conditioned {
                let mut ids: Vec<NodeId> = layer
                    .values()
                    .flatten()
                    .map(|neighbor| neighbor.node)
                    .collect();
                ids.sort();
                ids.dedup();
                let vectors = self.vectors_for(&ids).await?;
                let query = query.expect("conditioned spread has a query");
                Some(
                    ids.into_iter()
                        .map(|id| {
                            let sim = vectors
                                .get(&id)
                                .map_or(0.0, |vector| crate::cosine(query, vector));
                            (id, crate::conditioning_factor(gamma, sim))
                        })
                        .collect::<HashMap<_, _>>(),
                )
            } else {
                None
            };

            for (source, neighbors) in &mut layer {
                if let Some(biases) = routing_biases.filter(|biases| !biases.is_empty()) {
                    neighbors.sort_by(|a, b| {
                        let a_factor = conditioning.as_ref().map_or(1.0, |factors| {
                            factors.get(&a.node).copied().unwrap_or(1.0 - gamma)
                        });
                        let b_factor = conditioning.as_ref().map_or(1.0, |factors| {
                            factors.get(&b.node).copied().unwrap_or(1.0 - gamma)
                        });
                        crate::routing_probe::biased_neighbor_order(
                            *source, a, a_factor, b, b_factor, biases,
                        )
                    });
                } else if let Some(conditioning) = &conditioning {
                    neighbors.sort_by(|a, b| {
                        let a_score = a.edge.weight()
                            * conditioning.get(&a.node).copied().unwrap_or(1.0 - gamma);
                        let b_score = b.edge.weight()
                            * conditioning.get(&b.node).copied().unwrap_or(1.0 - gamma);
                        b_score
                            .total_cmp(&a_score)
                            .then_with(|| crate::neighbor_order(a, b))
                    });
                }
                neighbors.truncate(SPREAD_FANOUT);
            }

            let mut proposals: HashMap<NodeId, f32> = HashMap::new();
            let mut proposed_paths = BTreeMap::new();
            for source in sources {
                for neighbor in layer.get(&source.id).map_or(&[][..], Vec::as_slice) {
                    let factor = conditioning.as_ref().map_or(1.0, |factors| {
                        factors.get(&neighbor.node).copied().unwrap_or(1.0 - gamma)
                    });
                    let original = source.score * neighbor.edge.weight() * factor;
                    crate::record_routing_order(
                        &mut ordering,
                        routing_biases,
                        source,
                        neighbor,
                        original,
                        budget.min_relevance,
                        &paths,
                    );
                    let mut propagated = original;
                    if propagated < budget.min_relevance {
                        // ε-exploration: occasionally follow an under-weighted
                        // edge, admitting it at the relevance floor.
                        if budget.explore > 0.0
                            && crate::explore_draw(source.id, neighbor.node) < budget.explore
                        {
                            propagated = budget.min_relevance;
                        } else {
                            continue;
                        }
                    }
                    if propagated > best.get(&neighbor.node).copied().unwrap_or(0.0)
                        && propagated > proposals.get(&neighbor.node).copied().unwrap_or(0.0)
                    {
                        crate::propose_score(&mut proposals, neighbor.node, propagated);
                        if let Some(paths) = &paths {
                            // Copy now: a predecessor can later acquire another
                            // winning route without having propagated it here.
                            let mut path = paths.get(&source.id).cloned().unwrap_or_default();
                            path.push(mneme_core::ports::TraversalHop {
                                previous: source.id,
                                target: neighbor.node,
                                edge: neighbor.edge.clone(),
                            });
                            proposed_paths.insert(neighbor.node, path);
                        }
                    }
                }
            }
            frontier = crate::admit_layer_routed(
                &mut best,
                proposals,
                budget.max_nodes,
                ordering.as_ref(),
            );
            if let Some(ordering) = &mut ordering {
                ordering.retain(|id, _| best.contains_key(id));
            }
            if let Some(paths) = &mut paths {
                for id in frontier.keys() {
                    if let Some(path) = proposed_paths.remove(id) {
                        paths.insert(*id, path);
                    }
                }
            }
        }

        let mut out: Vec<Scored> = best
            .into_iter()
            .map(|(id, score)| Scored { id, score })
            .collect();
        out.sort_by(crate::scored_order);
        Ok(mneme_core::ports::SpreadResult {
            hits: out,
            paths: trace.then_some(paths).flatten(),
            ordering,
        })
    }
}

#[async_trait]
impl Traversal for CozoStore {
    async fn spread(
        &self,
        seeds: &[Scored],
        budget: Budget,
        query: Option<&[f32]>,
        scope: TraversalScope,
    ) -> Result<Vec<Scored>> {
        Ok(self
            .spread_inner(seeds, budget, query, scope, false, None)
            .await?
            .hits)
    }

    async fn spread_with_provenance(
        &self,
        seeds: &[Scored],
        budget: Budget,
        query: Option<&[f32]>,
        scope: TraversalScope,
    ) -> Result<mneme_core::ports::SpreadResult> {
        self.spread_inner(seeds, budget, query, scope, true, None)
            .await
    }

    async fn spread_routed(
        &self,
        seeds: &[Scored],
        budget: Budget,
        query: Option<&[f32]>,
        scope: TraversalScope,
        biases: &mneme_core::ports::RoutingBiasMap,
        trace: bool,
    ) -> Result<mneme_core::ports::SpreadResult> {
        self.spread_inner(seeds, budget, query, scope, trace, Some(biases))
            .await
    }

    async fn detect_communities(&self, _: ColdPath) -> Result<Vec<(NodeId, ClusterId)>> {
        // All node ids first, so isolated nodes (absent from the edge relation,
        // hence unseen by Louvain) still get a label below.
        let node_rows = self.run(
            "?[id, data, status] := *node{id, data, status}",
            BTreeMap::new(),
            false,
        )?;
        let mut ids: Vec<NodeId> = node_rows
            .rows
            .iter()
            .map(|row| decode_canonical_node_row(row))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(Node::is_semantic)
            .map(|node| node.id())
            .collect();
        ids.sort();
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let index: HashMap<NodeId, usize> =
            ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();

        // Symmetric, weight-aggregated adjacency for weighted Louvain: each directed
        // edge contributes its weight to *both* endpoints' view (undirected), and a
        // pair edged both ways sums. Modularity over these weights is what keeps a
        // hub's many low-weight edges from collapsing the graph into one community.
        let edge_rows = self.run(
            "?[from, to, weight] := *edge{from, to, weight}",
            BTreeMap::new(),
            false,
        )?;
        let mut adj_w: Vec<HashMap<usize, f32>> = vec![HashMap::new(); ids.len()];
        for r in &edge_rows.rows {
            let from = node_id(want_str(&r[0])?)?;
            let to = node_id(want_str(&r[1])?)?;
            let w = want_f64(&r[2])? as f32;
            if let (Some(&a), Some(&b)) = (index.get(&from), index.get(&to)) {
                *adj_w[a].entry(b).or_insert(0.0) += w;
                *adj_w[b].entry(a).or_insert(0.0) += w;
            }
        }
        let adj: Vec<Vec<(usize, f32)>> =
            adj_w.into_iter().map(|m| m.into_iter().collect()).collect();

        // Isolated nodes have empty adjacency → Louvain leaves them as singletons →
        // densify gives each its own cluster id. No special-casing needed.
        let comm = crate::weighted_louvain(ids.len(), &adj);
        Ok(crate::densify(&ids, &comm))
    }
}

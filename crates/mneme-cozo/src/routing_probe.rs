//! Private, query-local ordering probe. This is not stored learning, feedback
//! authority, or a reader-visible score. The public traversal passes no map.

use mneme_core::NodeId;
use mneme_core::ports::Neighbor;
#[cfg(test)]
pub(crate) use mneme_core::ports::SignedRoutingBias;
pub(crate) use mneme_core::ports::{RoutingBiasMap, RoutingRoute};
use std::cmp::Ordering;

fn ordering_score(
    previous: NodeId,
    neighbor: &Neighbor,
    conditioning: f32,
    biases: &RoutingBiasMap,
) -> f32 {
    let weight = neighbor.edge.weight();
    // A zero-weight or zero-conditioned edge cannot be rescued by the ranking probe. The actual
    // propagation still uses the unchanged stored weight in either case.
    let base = weight * conditioning;
    if base <= 0.0 {
        return base;
    }
    biases
        .get(&RoutingRoute::from_neighbor(previous, neighbor))
        .map_or(base, |bias| base + bias.value())
}

/// The same ordering at the raw-32 and conditioned-final-8 cutoffs. No-map
/// callers retain their original comparator directly instead of passing here.
pub(crate) fn biased_neighbor_order(
    previous: NodeId,
    a: &Neighbor,
    a_conditioning: f32,
    b: &Neighbor,
    b_conditioning: f32,
    biases: &RoutingBiasMap,
) -> Ordering {
    let a_score = ordering_score(previous, a, a_conditioning, biases);
    let b_score = ordering_score(previous, b, b_conditioning, biases);
    b_score
        .total_cmp(&a_score)
        .then_with(|| {
            (b.edge.weight() * b_conditioning).total_cmp(&(a.edge.weight() * a_conditioning))
        })
        .then_with(|| crate::neighbor_order(a, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mneme_core::{Edge, EdgeKind};
    use ulid::Ulid;

    #[test]
    fn route_key_is_directional_zero_edges_stay_zero_and_ties_are_stable() {
        let [root, a, b] = [1u128, 2, 3].map(|id| NodeId(Ulid::from(id)));
        let first = Neighbor {
            edge: Edge::new(root, a, 0.5, EdgeKind::Associative, 1),
            node: a,
            incoming: false,
        };
        let second = Neighbor {
            edge: Edge::new(root, b, 0.5, EdgeKind::Associative, 1),
            node: b,
            incoming: false,
        };
        let mut biases = RoutingBiasMap::new();
        biases.insert(
            RoutingRoute::from_neighbor(root, &first),
            SignedRoutingBias::Boost,
        );
        biases.insert(
            RoutingRoute::from_neighbor(root, &second),
            SignedRoutingBias::Boost,
        );
        assert_eq!(
            biased_neighbor_order(root, &first, 1.0, &second, 1.0, &biases),
            Ordering::Less,
            "equal biased scores use deterministic node IDs"
        );
        let reverse = Neighbor {
            edge: first.edge.clone(),
            node: root,
            incoming: true,
        };
        assert_eq!(ordering_score(a, &reverse, 1.0, &biases), 0.5);
        let swapped_edge = Neighbor {
            edge: Edge::new(a, root, 0.5, EdgeKind::Associative, 1),
            node: a,
            incoming: true,
        };
        assert_eq!(
            ordering_score(root, &swapped_edge, 1.0, &biases),
            0.5,
            "identical traversal endpoints do not inherit another stored edge's bias"
        );
        let zero = Neighbor {
            edge: Edge::new(root, a, 0.0, EdgeKind::Transition, 1),
            node: a,
            incoming: false,
        };
        assert_eq!(ordering_score(root, &zero, 1.0, &biases), 0.0);
        assert_eq!(
            ordering_score(root, &first, 0.0, &biases),
            0.0,
            "zero conditioning receives no priority even on a positive edge"
        );
    }
    #[test]
    fn conditioned_priority_tie_prefers_original_contribution() {
        let [root, a, b] = [1u128, 2, 3].map(|id| NodeId(Ulid::from(id)));
        let first = Neighbor {
            node: a,
            incoming: false,
            edge: Edge::new(root, a, 0.7, EdgeKind::Transition, 1),
        };
        let second = Neighbor {
            node: b,
            incoming: false,
            edge: Edge::new(root, b, 0.6, EdgeKind::Transition, 1),
        };
        let biases = RoutingBiasMap::from([(
            RoutingRoute::from_neighbor(root, &first),
            SignedRoutingBias::Boost,
        )]);
        assert_eq!(
            ordering_score(root, &first, 0.5, &biases),
            ordering_score(root, &second, 1.0, &biases)
        );
        assert_eq!(
            biased_neighbor_order(root, &first, 0.5, &second, 1.0, &biases),
            Ordering::Greater
        );
    }
}

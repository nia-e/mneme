//! The startup summary counts a bounded physical prefix, not a filtered list of
//! open pairs. Both adapters must preserve that distinction without mutating it.

use std::time::Duration;

use mneme_core::ports::{
    CONTRADICTION_SUMMARY_TIMEOUT, ContradictionSummary, ContradictionSummaryRequest, GraphStore,
    MAX_CONTRADICTION_SUMMARY_SCAN_ROWS,
};
use mneme_core::{BodyRef, Node, NodeId, NodeStatus, Provenance, Resolution};
use ulid::Ulid;
use unordered_pair::UnorderedPair;

const DIM: usize = 8;

fn node(id: u128) -> Node {
    Node::try_new(
        NodeId(Ulid::from(id)),
        format!("claim {id}"),
        BodyRef::new("inline://summary-test").unwrap(),
        std::iter::empty::<&str>(),
        Provenance::derived_empty(),
        0.5,
        0.5,
        NodeStatus::Active,
        1_000,
    )
    .unwrap()
}

async fn pair(
    store: &impl GraphStore,
    first: u128,
    resolution: Option<Resolution>,
) -> UnorderedPair<NodeId> {
    let a = node(first);
    let b = node(first + 1);
    store.put_node(&a).await.unwrap();
    store.put_node(&b).await.unwrap();
    store
        .observe_contradiction(a.id(), b.id(), 1_001)
        .await
        .unwrap();
    let pair = UnorderedPair(a.id(), b.id());
    if let Some(resolution) = resolution {
        store.resolve_contradiction(pair, resolution).await.unwrap();
    }
    pair
}

async fn summary(store: &impl GraphStore, scan_rows: usize) -> ContradictionSummary {
    store
        .contradiction_summary(
            &ContradictionSummaryRequest::new(scan_rows, CONTRADICTION_SUMMARY_TIMEOUT).unwrap(),
        )
        .await
        .unwrap()
        .expect("both built-in adapters support the bounded summary")
}

// These are the same port-level promises on independent backend mechanisms.
macro_rules! backend_contract_tests {
    ($new:expr) => {
        #[tokio::test]
        async fn empty_is_exact_zero() {
            let store = $new;
            let result = summary(&store, MAX_CONTRADICTION_SUMMARY_SCAN_ROWS).await;
            assert_eq!(result.open_found(), 0);
            assert_eq!(result.rows_scanned(), 0);
            assert!(result.complete());
        }

        #[tokio::test]
        async fn mixed_verdicts_and_duplicate_observations_count_pairs() {
            let store = $new;
            let open = pair(&store, 10, None).await;
            pair(&store, 20, Some(Resolution::Unresolved)).await;
            pair(&store, 30, Some(Resolution::ContextDependent)).await;
            pair(&store, 40, Some(Resolution::Superseded)).await;
            store
                .observe_contradiction(open.1, open.0, 1_002)
                .await
                .unwrap();

            let result = summary(&store, MAX_CONTRADICTION_SUMMARY_SCAN_ROWS).await;
            assert_eq!(result.open_found(), 2, "Unresolved remains open");
            assert_eq!(result.rows_scanned(), 4, "repeat observations are not rows");
            assert!(result.complete());
        }

        #[tokio::test]
        async fn terminal_rows_consume_the_physical_scan_allowance() {
            let store = $new;
            // Reference HashMap order is deliberately unspecified. Every row is
            // terminal, so any physical prefix must have the same count.
            for first in [10, 20, 30] {
                pair(&store, first, Some(Resolution::ContextDependent)).await;
            }

            let result = summary(&store, 2).await;
            assert_eq!(result.open_found(), 0);
            assert_eq!(result.rows_scanned(), 2);
            assert!(!result.complete(), "partial zero is not an all-clear");
        }

        #[tokio::test]
        async fn open_rows_are_a_lower_bound_until_coverage_is_complete() {
            let store = $new;
            for first in [10, 20, 30] {
                pair(&store, first, None).await;
            }

            let bounded = summary(&store, 2).await;
            assert_eq!(bounded.open_found(), 2);
            assert_eq!(bounded.rows_scanned(), 2);
            assert!(!bounded.complete());

            let complete = summary(&store, MAX_CONTRADICTION_SUMMARY_SCAN_ROWS).await;
            assert_eq!(complete.open_found(), 3);
            assert_eq!(complete.rows_scanned(), 3);
            assert!(complete.complete());
        }

        #[tokio::test]
        async fn exactly_full_prefix_does_not_claim_an_unbudgeted_end_probe() {
            let store = $new;
            pair(&store, 10, None).await;

            let full = summary(&store, 1).await;
            assert_eq!(full.open_found(), 1);
            assert_eq!(full.rows_scanned(), 1);
            assert!(!full.complete());

            let complete = summary(&store, 2).await;
            assert_eq!(complete.open_found(), 1);
            assert_eq!(complete.rows_scanned(), 1);
            assert!(complete.complete());
        }

        #[tokio::test]
        async fn summary_does_not_change_the_exported_graph() {
            let store = $new;
            let open = pair(&store, 10, None).await;
            pair(&store, 20, Some(Resolution::Unresolved)).await;
            pair(&store, 30, Some(Resolution::ContextDependent)).await;
            store
                .observe_contradiction(open.1, open.0, 1_002)
                .await
                .unwrap();
            let before = snapshot(&store).await;

            summary(&store, 1).await;
            summary(&store, MAX_CONTRADICTION_SUMMARY_SCAN_ROWS).await;

            assert_eq!(snapshot(&store).await, before);
        }

        #[tokio::test]
        async fn exhausted_deadline_is_unavailable_not_zero() {
            let store = $new;
            pair(&store, 10, None).await;
            let request = ContradictionSummaryRequest::new(1, Duration::from_nanos(1)).unwrap();
            assert!(store.contradiction_summary(&request).await.is_err());
        }
    };
}

mod reference {
    use super::*;
    use mneme_cozo::MemStore;

    async fn snapshot(store: &MemStore) -> serde_json::Value {
        serde_json::to_value(store.export()).unwrap()
    }

    backend_contract_tests!(MemStore::new(DIM));
}

#[cfg(feature = "cozo")]
mod cozo {
    use super::*;
    use mneme_cozo::CozoStore;

    async fn snapshot(store: &CozoStore) -> serde_json::Value {
        serde_json::to_value(store.export().await.unwrap()).unwrap()
    }

    backend_contract_tests!(CozoStore::new(DIM).unwrap());

    #[tokio::test]
    async fn open_pair_after_terminal_prefix_is_not_scanned_past_the_allowance() {
        let store = CozoStore::new(DIM).unwrap();
        // Canonical relation order is (lo, hi). A filter-before-limit query
        // would incorrectly reach the final open row through this prefix.
        pair(&store, 10, Some(Resolution::ContextDependent)).await;
        pair(&store, 20, Some(Resolution::Superseded)).await;
        pair(&store, 30, None).await;

        let bounded = summary(&store, 2).await;
        assert_eq!(bounded.open_found(), 0);
        assert_eq!(bounded.rows_scanned(), 2);
        assert!(!bounded.complete());

        let complete = summary(&store, MAX_CONTRADICTION_SUMMARY_SCAN_ROWS).await;
        assert_eq!(complete.open_found(), 1);
        assert_eq!(complete.rows_scanned(), 3);
        assert!(complete.complete());
    }
}

//! Read-only graph presentation support. Indexed topology scans do not resolve
//! bodies, rank memories, or emit learning evidence.
use crate::Memory;
use mneme_core::ports::{
    ColdPath, Error, MAX_MAINTENANCE_BATCH_ROWS, MaintenanceEdgeKey, MaintenanceEdgePage, Result,
};
use mneme_core::{Node, NodeId};

impl Memory {
    pub async fn graph_edge_upper_bound(&self) -> Result<Option<MaintenanceEdgeKey>> {
        self.graph
            .maintenance_edge_upper_bound(ColdPath::acquire())
            .await
    }

    pub async fn graph_edges_page(
        &self,
        after: Option<MaintenanceEdgeKey>,
        through: MaintenanceEdgeKey,
        limit: usize,
    ) -> Result<MaintenanceEdgePage> {
        if !(1..=MAX_MAINTENANCE_BATCH_ROWS).contains(&limit) {
            return Err(Error::InvalidInput(
                "graph edge page limit must be 1..=64".into(),
            ));
        }
        self.graph
            .maintenance_edges_page(ColdPath::acquire(), after, through, limit)
            .await
    }

    /// One batch fetch of exact canonical identities, including historical
    /// editions and explicit missing entries; never substitutes episode heads.
    pub async fn graph_summary_nodes(&self, ids: &[NodeId]) -> Result<Vec<Option<Node>>> {
        if ids.is_empty() || ids.len() > MAX_MAINTENANCE_BATCH_ROWS {
            return Err(Error::InvalidInput(
                "graph summary batch must contain 1..=64 identities".into(),
            ));
        }
        let _snapshot = self.mutation_gate.lock().await;
        let nodes = self.graph.get_nodes(ids).await?;
        if nodes.len() != ids.len()
            || nodes
                .iter()
                .zip(ids)
                .any(|(node, id)| node.as_ref().is_some_and(|node| node.id() != *id))
        {
            return Err(Error::Backend(
                "graph summary backend did not preserve requested identity slots".into(),
            ));
        }
        Ok(nodes)
    }
}

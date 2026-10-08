//! Bounded raw adjacency inspection, distinct from relevance-ranked traversal.
use crate::{HydratedNeighbor, Memory};
use mneme_core::ports::{
    Error, IncidentEdgeReadWork, IncidentEdgesCursor, IncidentEdgesRequest, Neighbor, Result,
};

#[derive(Clone, Debug)]
pub struct NeighborInspectionPage {
    pub items: Vec<HydratedNeighbor>,
    pub next: Option<IncidentEdgesCursor>,
    pub work: IncidentEdgeReadWork,
}

impl Memory {
    /// Read at most one physical indexed incident page and hydrate at most 64
    /// endpoint slots in one mutation snapshot. Raw episode/dangling edges are
    /// retained. No relevance ranking, inference, reinforcement or body reads.
    /// Physical self-edges may occur in both legs, as in the indexed port.
    pub async fn inspect_neighbors_page(
        &self,
        request: &IncidentEdgesRequest,
    ) -> Result<NeighborInspectionPage> {
        request.validate()?;
        let started = std::time::Instant::now();
        let _snapshot = self.mutation_gate.lock().await;
        let remaining = request
            .remaining()
            .checked_sub(started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| Error::Backend("neighbor inspection deadline exhausted".into()))?;
        let bounded = IncidentEdgesRequest::new(
            request.anchor(),
            request.scan_rows(),
            remaining,
            request.after().cloned(),
        )?;
        let page = self
            .graph
            .incident_edges_page(&bounded)
            .await?
            .ok_or_else(|| {
                Error::Backend("graph adapter does not support indexed neighbor inspection".into())
            })?;
        if page.items.len() > request.scan_rows() || page.work.rows_scanned > request.scan_rows() {
            return Err(Error::Backend(
                "incident backend exceeded neighbor page bound".into(),
            ));
        }
        if let Some(next) = &page.next {
            next.validate(request.anchor())?;
            if next.is_complete() || request.after() == Some(next) {
                return Err(Error::Backend(
                    "incident backend returned a non-progressing continuation".into(),
                ));
            }
        }
        let mut neighbors = Vec::with_capacity(page.items.len());
        for edge in page.items {
            let incoming = edge.from != request.anchor();
            if incoming && edge.to != request.anchor() {
                return Err(Error::Backend(
                    "incident backend escaped neighbor anchor".into(),
                ));
            }
            neighbors.push(Neighbor {
                node: if incoming { edge.from } else { edge.to },
                edge,
                incoming,
            });
        }
        Ok(NeighborInspectionPage {
            items: self.hydrate_neighbor_rows(neighbors).await?,
            next: page.next,
            work: page.work,
        })
    }
}

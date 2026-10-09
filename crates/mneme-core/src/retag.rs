//! Optional canonical-content guards for classifier-authored tag maintenance.
use crate::{
    Node, NodeId,
    ports::{Error, Result, routing_content_fingerprint},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeContentGuard {
    pub id: NodeId,
    pub content_fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetagContentGuards {
    pub target_fingerprint: String,
    pub guard_nodes: Vec<NodeContentGuard>,
}
fn valid_fingerprint(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl RetagContentGuards {
    pub fn validate(&self) -> Result<()> {
        if !valid_fingerprint(&self.target_fingerprint)
            || self
                .guard_nodes
                .iter()
                .any(|g| !valid_fingerprint(&g.content_fingerprint))
        {
            return Err(Error::InvalidInput(
                "content fingerprint must be canonical lowercase SHA-256".into(),
            ));
        }
        if self.guard_nodes.len() > crate::MAX_NODE_HYDRATION_BATCH {
            return Err(Error::CapacityExceeded {
                resource: "retag guard nodes",
                limit: crate::MAX_NODE_HYDRATION_BATCH,
            });
        }
        let mut ids = std::collections::HashSet::new();
        if self.guard_nodes.iter().any(|g| !ids.insert(g.id)) {
            return Err(Error::InvalidInput(
                "retag guards contain duplicate node identities".into(),
            ));
        }
        Ok(())
    }
    /// Binds canonical meaning and pointers, NOT mutable external body bytes.
    pub fn check_target(&self, target: &Node) -> Result<()> {
        if routing_content_fingerprint(target) != self.target_fingerprint {
            return Err(Error::Conflict(
                "node content changed; inspect before retagging".into(),
            ));
        }
        Ok(())
    }
}
impl NodeContentGuard {
    pub fn check(&self, node: Option<&Node>) -> Result<()> {
        if node.is_none_or(|n| {
            n.id() != self.id
                || !n.is_semantic()
                || routing_content_fingerprint(n) != self.content_fingerprint
        }) {
            return Err(Error::Conflict("retag guide content changed, disappeared or is not a semantic node; inspect before retrying".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_canonical_guard_admission() {
        let id = NodeId(ulid::Ulid::new());
        let guard = NodeContentGuard {
            id,
            content_fingerprint: "a".repeat(64),
        };
        let mut request = RetagContentGuards {
            target_fingerprint: "0".repeat(64),
            guard_nodes: vec![guard.clone()],
        };
        assert!(request.validate().is_ok());
        request.guard_nodes.push(guard.clone());
        assert!(request.validate().is_err());
        request.guard_nodes.clear();
        for fingerprint in ["A".repeat(64), "a".repeat(63), "g".repeat(64)] {
            request.target_fingerprint = fingerprint;
            assert!(request.validate().is_err());
        }
        request.target_fingerprint = "0".repeat(64);
        request.guard_nodes = (0..=crate::MAX_NODE_HYDRATION_BATCH)
            .map(|_| NodeContentGuard {
                id: NodeId(ulid::Ulid::new()),
                ..guard.clone()
            })
            .collect();
        assert!(request.validate().is_err());
    }
}

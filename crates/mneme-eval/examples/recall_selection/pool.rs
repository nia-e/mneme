use mneme_core::{EmbeddingFingerprint, NodeId};
use mneme_present::{
    BodyBudget, CoreInputCard, ExpansionInputCard, LaneBudgets, LaneLimit, LaneWindow,
    PackingInput, PresentationBudget, PrimaryInputCard, ProbationaryInputCard, RetrievalMetadata,
    RetrievalStamp, pack,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::num::{NonZeroU16, NonZeroU32};

pub const POOL_SCHEMA: &str = "mneme.recall-selection.pool.v1";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pool {
    pub schema: String,
    pub split: String,
    pub cases: Vec<PoolCase>,
    #[serde(default)]
    pub setup_ms: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PoolCase {
    pub case_id: String,
    pub variant: String,
    pub query: String,
    pub budget_bytes: u32,
    pub corpus_keys: Vec<String>,
    pub candidates: Vec<Candidate>,
    pub stamp: Stamp,
    #[serde(default)]
    pub acquisition_ms: f64,
    /// Extra document-embedding inference required by MMR, measured during
    /// acquisition and charged per logical policy invocation in replay.
    #[serde(default)]
    pub feature_ms: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub node_key: String,
    pub id: NodeId,
    pub lane: String,
    pub source_rank: u16,
    pub summary: String,
    pub embedding: Vec<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Stamp {
    pub policy_contract: String,
    pub policy_fingerprint: [u8; 32],
    pub embedding: EmbeddingFingerprint,
    pub vector_semantics: String,
    pub lexical_semantics: Option<String>,
    pub reranker_semantics: Option<String>,
}

impl Pool {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != POOL_SCHEMA || !["dev", "test"].contains(&self.split.as_str()) {
            return Err("invalid pool schema or split".into());
        }
        let mut cases = HashSet::new();
        for case in &self.cases {
            if !["original", "reversed"].contains(&case.variant.as_str())
                || !cases.insert((&case.case_id, &case.variant))
            {
                return Err("invalid or duplicate case variant".into());
            }
            if case.query.trim().is_empty() || ![4096, 8192].contains(&case.budget_bytes) {
                return Err(format!("invalid query or budget in {}", case.case_id));
            }
            let mut ids = HashSet::new();
            let mut keys = HashSet::new();
            let mut last = BTreeMap::new();
            for c in &case.candidates {
                if !["primary", "probationary"].contains(&c.lane.as_str())
                    || c.source_rank == 0
                    || !ids.insert(c.id)
                    || !keys.insert(&c.node_key)
                {
                    return Err(format!(
                        "invalid or duplicate candidate in {}",
                        case.case_id
                    ));
                }
                if let Some(prev) = last.insert(c.lane.as_str(), c.source_rank) {
                    if c.source_rank <= prev {
                        return Err(format!("nonascending source ranks in {}", case.case_id));
                    }
                }
                if c.embedding.len() != case.stamp.embedding.dimension
                    || c.embedding.iter().any(|x| !x.is_finite())
                {
                    return Err(format!("invalid embedding in {}", case.case_id));
                }
            }
        }
        let mut paired: BTreeMap<&str, (&PoolCase, &PoolCase)> = BTreeMap::new();
        for case in self.cases.iter().filter(|c| c.variant == "original") {
            let other = self
                .cases
                .iter()
                .find(|c| c.case_id == case.case_id && c.variant == "reversed")
                .ok_or_else(|| format!("missing reversed pair for {}", case.case_id))?;
            paired.insert(&case.case_id, (case, other));
        }
        if paired.len() * 2 != self.cases.len() {
            return Err("unpaired case variant".into());
        }
        for (id, (a, b)) in paired {
            if a.query != b.query
                || a.budget_bytes != b.budget_bytes
                || a.corpus_keys != b.corpus_keys
                || serde_json::to_value(&a.candidates).map_err(|e| e.to_string())?
                    != serde_json::to_value(&b.candidates).map_err(|e| e.to_string())?
                || serde_json::to_value(&a.stamp).map_err(|e| e.to_string())?
                    != serde_json::to_value(&b.stamp).map_err(|e| e.to_string())?
            {
                return Err(format!("paired pool differs before presentation: {id}"));
            }
        }
        Ok(())
    }
}

impl PoolCase {
    pub fn primary(&self) -> Vec<&Candidate> {
        self.candidates
            .iter()
            .filter(|c| c.lane == "primary")
            .collect()
    }

    pub fn probationary(&self) -> Vec<&Candidate> {
        self.candidates
            .iter()
            .filter(|c| c.lane == "probationary")
            .collect()
    }

    pub fn retrieval_metadata(&self) -> Result<RetrievalMetadata, String> {
        let s = &self.stamp;
        let stamp = RetrievalStamp::new(
            &s.policy_contract,
            s.policy_fingerprint,
            s.embedding.clone(),
            &s.vector_semantics,
            s.lexical_semantics.clone(),
            s.reranker_semantics.clone(),
            Vec::new(),
        )
        .map_err(|e| e.to_string())?;
        RetrievalMetadata::untagged(stamp).map_err(|e| e.to_string())
    }

    /// Packer sees only a policy-selected subset, in ascending *original*
    /// source rank. Selection omissions are recorded separately by the caller;
    /// this does not rewrite the retrieval stamp or fabricate presentation ranks.
    pub fn pack_subset(
        &self,
        primary_indices: &[usize],
    ) -> Result<mneme_present::PackPlan, String> {
        let primary = self.primary();
        let mut unique = HashSet::new();
        if primary_indices
            .iter()
            .any(|&i| i >= primary.len() || !unique.insert(i))
        {
            return Err("invalid selected primary index".into());
        }
        let mut selected = primary_indices.to_vec();
        selected.sort_by_key(|&i| primary[i].source_rank);
        let primary_cards = selected
            .iter()
            .map(|&i| {
                let c = primary[i];
                PrimaryInputCard::new(c.id, NonZeroU16::new(c.source_rank).unwrap(), &c.summary)
                    .map_err(|e| e.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let probationary_cards = self
            .probationary()
            .iter()
            .map(|c| {
                ProbationaryInputCard::new(
                    c.id,
                    NonZeroU16::new(c.source_rank).unwrap(),
                    &c.summary,
                )
                .map_err(|e| e.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let input = PackingInput::new(
            self.retrieval_metadata()?,
            LaneWindow::<CoreInputCard>::complete(Vec::new()),
            LaneWindow::bounded(primary_cards, true),
            LaneWindow::bounded(probationary_cards, true),
            LaneWindow::<ExpansionInputCard>::complete(Vec::new()),
        )
        .map_err(|e| e.to_string())?;
        pack(&presentation_budget(self.budget_bytes)?, &input).map_err(|e| e.to_string())
    }
}

fn presentation_budget(bytes: u32) -> Result<PresentationBudget, String> {
    let control = PresentationBudget::minimum_control_reserve_bytes();
    let lane_bytes = bytes
        .checked_sub(control)
        .ok_or("budget below control reserve")?;
    let disabled = LaneLimit::new(0, 0, 0, 0).map_err(|e| e.to_string())?;
    let primary = LaneLimit::new(1, 12, 12, lane_bytes).map_err(|e| e.to_string())?;
    let probationary = LaneLimit::new(0, 1, 1, lane_bytes).map_err(|e| e.to_string())?;
    PresentationBudget::new(
        NonZeroU32::new(bytes).ok_or("zero presentation budget")?,
        NonZeroU32::new(control).ok_or("zero control reserve")?,
        NonZeroU16::new(13).unwrap(),
        NonZeroU16::new(2048).unwrap(),
        BodyBudget::disabled(),
        LaneBudgets::new(disabled, primary, probationary, disabled),
    )
    .map_err(|e| e.to_string())
}
